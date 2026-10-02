mod geometry;
mod gpu;
mod ipc;
mod mask;
mod model;
mod motion;
mod native;
mod pixels;
mod renderer;
mod telemetry;

use ene_protocol::avatar::{AvatarEvent, AvatarRequest, AvatarSettings, MotionState};
use smithay_client_toolkit::{
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    shell::WaylandSurface,
};
use std::{
    error::Error,
    time::{Duration, Instant},
};
use wayland_client::{Connection, globals::registry_queue_init};

use bevy::render::renderer::{RenderAdapter, RenderDevice, RenderInstance, RenderQueue};
use ipc::emit;
use std::{collections::BTreeMap, sync::Arc};
use telemetry::{CpuFrameDurations, duration_ns};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Gpu,
    Geometry,
    Empty,
    Shm,
}
impl Backend {
    fn from_env() -> Result<Self, Box<dyn Error>> {
        Ok(
            match std::env::var("ENE_AVATAR_BACKEND")
                .as_deref()
                .unwrap_or("gpu")
            {
                "gpu" => Self::Gpu,
                "geometry" => Self::Geometry,
                "empty" => Self::Empty,
                "shm" => Self::Shm,
                _ => return Err("ENE_AVATAR_BACKEND must be gpu, geometry, empty or shm".into()),
            },
        )
    }
    fn name(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Geometry => "geometry",
            Self::Empty => "empty",
            Self::Shm => "shm",
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    use clap::Parser;
    let args = Args::parse();
    let mut ipc = ipc::Ipc::new(args.standalone);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(args, &mut ipc)));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("ene-avatar: {error}");
            emit(AvatarEvent::Error {
                message: error.to_string(),
            });
            drop(ipc);
            std::process::exit(1);
        }
        Err(_) => {
            emit(AvatarEvent::Error{message:"キャラクターの描画処理に失敗しました。GPU・モデルを確認して再起動してください。".into()});
            drop(ipc);
            std::process::exit(1);
        }
    }
}
#[derive(clap::Parser)]
#[command(about = "Wayland layer-shell VRM renderer; normally controlled by ene-desktop")]
struct Args {
    #[arg(long)]
    standalone: bool,
    #[arg(long, required_if_eq("standalone", "true"))]
    model: Option<String>,
    #[arg(long)]
    motion: Option<String>,
    #[arg(long)]
    no_loop: bool,
}
fn run(args: Args, ipc: &mut ipc::Ipc) -> Result<(), Box<dyn Error>> {
    let mut settings = AvatarSettings {
        model: args.model.unwrap_or_default(),
        motion: args.motion,
        motion_loop: !args.no_loop,
        ..AvatarSettings::default()
    };
    let rx = &mut ipc.requests;
    if !args.standalone {
        match rx.blocking_recv().flatten() {
            Some(AvatarRequest::Configure { settings: initial }) => settings = initial,
            _ => return Ok(()),
        }
    }
    settings.validate()?;
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init(&connection)?;
    let qh = queue.handle();
    let backend = Backend::from_env()?;
    let metrics = telemetry::Recorder::shared(backend.name());
    let mut state =
        native::State::new(&globals, &qh, settings, connection.clone(), metrics.clone())?;
    queue.roundtrip(&mut state)?;
    state.recreate(&qh);
    let mut event_loop = EventLoop::<native::State>::try_new()?;
    WaylandSource::new(connection.clone(), queue).insert(event_loop.handle())?;
    let mut renderer = None::<renderer::Renderer>;
    let mut next_frame = Instant::now();
    let mut gpu_frames = None::<gpu::GpuFrames>;
    let mut gpu_surface = None::<gpu::GpuSurface>;
    let mut frame_sequence = 0_u64;
    let mut cpu_frames = BTreeMap::<u64, CpuFrameDurations>::new();
    let mut verifications = Vec::<(gpu::Frame, gpu::VerificationReadback)>::new();
    let mut summary_at = Instant::now();

    let mut motion_state = None::<MotionState>;
    let mut revision = 0;
    let mut ready = false;
    let mut load_started = Instant::now();
    let mut hidden = false;
    let mut presented = false;
    let mut resume_pending = false;
    loop {
        let wait = if !state.settings.visible
            || !state.configured
            || renderer
                .as_ref()
                .is_some_and(|renderer| renderer.model_failed)
        {
            Duration::from_millis(100)
        } else {
            next_frame
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(2))
        };
        event_loop.dispatch(wait, &mut state)?;
        state.apply_drag();
        connection.flush()?;
        let mut remap = state.output_changed || state.closed;
        state.output_changed = false;
        let mut configure_renderer = false;
        let mut shutdown = false;
        loop {
            let request = match rx.try_recv() {
                Ok(request) => request,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            };
            match request {
                None | Some(AvatarRequest::Shutdown) => shutdown = true,
                Some(AvatarRequest::Recenter) => state.request_recenter(),
                Some(AvatarRequest::Configure { settings }) => {
                    if let Err(message) = settings.validate() {
                        emit(AvatarEvent::Error { message });
                        continue;
                    }
                    if state.settings.output != settings.output {
                        remap = true;
                    }
                    state.settings = settings;
                    configure_renderer = true;
                }
            }
        }
        if shutdown {
            break;
        }
        // Coalesce settings to the last request. Model/motion changes update
        // the existing asset server and GPU instead of rebuilding Bevy.
        if configure_renderer {
            if let Some(renderer) = renderer.as_mut() {
                match renderer.configure(&state.settings) {
                    Ok(true) => {
                        ready = false;
                        load_started = Instant::now();
                        resume_pending = true;
                    }
                    Ok(false) if ready => {
                        // Selecting the still-active model after a bad choice
                        // also clears the error without reloading or restarting.
                        emit(AvatarEvent::Ready {
                            model: renderer.credit.clone(),
                        });
                    }
                    Ok(false) => {}
                    Err(message) => emit(AvatarEvent::Error { message }),
                }
                motion_state = None;
            }
            if !remap {
                state.properties();
                state.layer.commit();
            }
        }
        if remap {
            // Layer-shell fixes the output at creation. Replace only this role;
            // the renderer, model and animation remain alive.
            gpu_surface = None;
            state.recreate(&qh);
            if let Some(renderer) = renderer.as_mut() {
                renderer.generation += 1;
            }
            presented = false;
            load_started = Instant::now();
            resume_pending = true;
        }
        state.apply_recenter();
        if !state.settings.visible {
            if !hidden {
                // Destroy the mapped role, including queued configure events,
                // rather than unmapping and acknowledging a reset serial.
                // Keep GPU/model resources; no updates run while hidden.
                gpu_surface = None;
                state.recreate(&qh);
                if let Some(renderer) = renderer.as_mut() {
                    renderer.drain_capture();
                }
                hidden = true;
                presented = false;
                emit(AvatarEvent::VisibilityChanged { visible: false });
            }
            continue;
        }
        if hidden {
            resume_pending = true;
            hidden = false;
            load_started = Instant::now();
        }
        if !state.configured {
            resume_pending = true;
            continue;
        }
        if renderer.is_none() {
            renderer = Some(renderer::Renderer::new(
                state.width,
                state.height,
                state.scale,
                &state.settings,
                state.native_window(),
            )?);
            let world = renderer.as_ref().unwrap().app.world();
            if backend != Backend::Shm {
                let mut frames = gpu::GpuFrames::new(
                    world.resource::<RenderDevice>().clone(),
                    world.resource::<RenderQueue>().clone(),
                );
                frames.set_profiling(metrics.lock().unwrap().enabled());
                gpu_frames = Some(frames);
            }
            load_started = Instant::now();
        }
        let renderer = renderer.as_mut().unwrap();
        if renderer.model_failed {
            continue;
        }
        if state.resized {
            renderer.resize(state.width, state.height, state.scale);
            state.resized = false;
        }
        if resume_pending {
            renderer.resume();
            load_started = Instant::now();
            resume_pending = false;
        }
        {
            for sample in renderer.poll_scene_timings() {
                metrics
                    .lock()
                    .unwrap()
                    .record_gpu_durations(telemetry::GpuDurations {
                        frame_id: sample.frame_id,
                        timestamp_readback_bytes: sample.timestamp_readback_bytes,
                        ..Default::default()
                    });
                metrics
                    .lock()
                    .unwrap()
                    .record_gpu_timestamps(telemetry::GpuTimestampSample {
                        frame_id: sample.frame_id,
                        timestamp_period_ns: sample.timestamp_period_ns,
                        scene_render_batch: sample.buffered.then_some(sample.span),
                        scene: (!sample.buffered).then_some(sample.span),
                        ..Default::default()
                    });
            }
        }
        if let Some(surface) = gpu_surface.as_mut() {
            for (frame_id, duration) in surface.poll_timings() {
                metrics
                    .lock()
                    .unwrap()
                    .record_gpu_durations(telemetry::GpuDurations {
                        frame_id,
                        present_ns: Some(duration_ns(duration)),
                        timestamp_readback_bytes: 16,
                        ..Default::default()
                    });
            }
            if let Some(error) = surface.take_errors().into_iter().next() {
                return Err(error.into());
            }
        }
        if backend != Backend::Shm {
            let device = renderer.app.world().resource::<RenderDevice>().clone();
            let queue_gpu = renderer.app.world().resource::<RenderQueue>().clone();
            let frames = gpu_frames.as_mut().unwrap();
            let mut completed = frames.poll();
            if let Some(error) = frames.take_errors().into_iter().next() {
                return Err(error.into());
            }
            let mut index = 0;
            while index < verifications.len() {
                if let Some(rgba) = verifications[index].1.poll(device.wgpu_device())? {
                    let (frame, _) = verifications.swap_remove(index);
                    let reference = mask::alpha_runs(&rgba, frame.width, frame.height, frame.scale);
                    let width = frame.width.div_ceil(frame.scale);
                    let height = frame.height.div_ceil(frame.scale);
                    let (fp, fn_) = mask::coverage_errors(&reference, &frame.runs, width, height);
                    let expected: u64 = reference.iter().map(|r| r.width as u64).sum();
                    let tp = expected - fn_;
                    metrics
                        .lock()
                        .unwrap()
                        .record_verification(telemetry::VerificationSample {
                            frame_id: frame.sequence,
                            width,
                            height,
                            scale: frame.scale as f64,
                            reference_rule: "any physical alpha byte > 8 in each logical pixel"
                                .into(),
                            true_positives: tp,
                            false_positive: fp,
                            false_negative: fn_,
                            true_negatives: u64::from(width) * u64::from(height) - tp - fp - fn_,
                        });
                } else {
                    index += 1;
                }
            }
            completed.retain(|frame| {
                frame.sequence > revision
                    && frame.scale == state.scale
                    && frame.generation == renderer.generation
                    && frame.width == state.width * state.scale
                    && frame.height == state.height * state.scale
            });
            if let Some(frame) = completed.pop() {
                if gpu_surface.is_none() {
                    let world = renderer.app.sub_app(bevy::render::RenderApp).world();
                    gpu_surface = Some(gpu::GpuSurface::new(
                        world.resource::<RenderInstance>(),
                        world.resource::<RenderAdapter>(),
                        device.wgpu_device(),
                        &queue_gpu,
                        Arc::new(state.native_window()),
                    )?);
                    gpu_surface
                        .as_mut()
                        .unwrap()
                        .set_profiling(metrics.lock().unwrap().enabled());
                    eprintln!(
                        "avatar GPU surface: {:?}, {:?}",
                        gpu_surface.as_ref().unwrap().format(),
                        gpu_surface.as_ref().unwrap().alpha_mode()
                    );
                }
                let present_started = Instant::now();
                let output = gpu_surface.as_mut().unwrap().draw_timed(
                    &frame.texture,
                    frame.width,
                    frame.height,
                    frame.sequence,
                )?;
                if let Some(output) = output {
                    let submit_ns = duration_ns(present_started.elapsed());
                    let input_started = Instant::now();
                    state.prepare_gpu_present(&frame.runs);
                    let input_ns = duration_ns(input_started.elapsed());
                    metrics
                        .lock()
                        .unwrap()
                        .frame_enqueued(frame.sequence, frame.timings.enqueued_at);
                    state.request_feedback(&qh, frame.sequence);
                    output.present();
                    revision = frame.sequence;
                    if metrics.lock().unwrap().verification_enabled() {
                        verifications.push((
                            frame.clone(),
                            frame.readback_rgba(device.wgpu_device(), &queue_gpu),
                        ));
                    }
                    let mut sample = cpu_frames.remove(&frame.sequence).unwrap_or_default();
                    debug_assert_eq!(frame.mask_readback, backend == Backend::Gpu);
                    sample.frame_id = frame.sequence;
                    sample.input_region_ns = Some(input_ns);
                    sample.mask_ns =
                        Some(sample.mask_ns.unwrap_or(0) + duration_ns(frame.timings.cpu_decode));
                    sample.submit_ns = Some(submit_ns + duration_ns(frame.timings.cpu_submit));
                    sample.decode_ns = Some(duration_ns(frame.timings.cpu_decode));
                    sample.coverage_ns = Some(
                        sample.mask_ns.unwrap_or(0)
                            + input_ns
                            + duration_ns(frame.timings.cpu_submit),
                    );
                    sample.total_ns += input_ns
                        + sample.submit_ns.unwrap_or(0)
                        + duration_ns(frame.timings.cpu_decode);
                    metrics.lock().unwrap().record_cpu_frame(sample);
                    metrics
                        .lock()
                        .unwrap()
                        .record_gpu_durations(telemetry::GpuDurations {
                            frame_id: frame.sequence,
                            transport_latency_ns: Some(duration_ns(
                                frame.timings.enqueued_at.elapsed(),
                            )),
                            mask_ns: frame.timings.gpu_mask.map(duration_ns),
                            copy_mask_ns: frame.timings.gpu_copy_mask.map(duration_ns),
                            readback_bytes: frame.timings.mask_readback_bytes,
                            timestamp_readback_bytes: frame.timings.timestamp_readback_bytes,
                            overflow_retries: frame.timings.overflow_retries,
                            ..Default::default()
                        });
                    if (!frame.runs.is_empty() || backend == Backend::Empty) && !ready {
                        emit(AvatarEvent::Ready {
                            model: renderer.credit.clone(),
                        });
                        ready = true;
                    }
                    // Empty-region benchmark has real visible pixels but deliberately no input.
                    if (!frame.runs.is_empty() || backend == Backend::Empty) && !presented {
                        emit(AvatarEvent::VisibilityChanged { visible: true });
                        presented = true;
                    }
                }
            }
        }
        if summary_at.elapsed() >= Duration::from_secs(1) {
            metrics.lock().unwrap().emit_summary();
            summary_at = Instant::now();
        }
        if Instant::now() < next_frame {
            continue;
        }
        let interval = Duration::from_secs_f64(1.0 / state.settings.fps as f64);
        next_frame += interval;
        if next_frame < Instant::now() {
            next_frame = Instant::now() + interval;
        }
        frame_sequence += 1;
        let work_started = Instant::now();
        let motion_started = Instant::now();
        renderer.update_motion(&state.settings);
        let motion_ns = duration_ns(motion_started.elapsed());
        if backend == Backend::Shm
            && let Some(sequence) = renderer.capture()
        {
            metrics
                .lock()
                .unwrap()
                .frame_enqueued(sequence, work_started);
        }
        let rendered_generation = renderer.generation;
        let update_started = Instant::now();
        renderer.profile_frame(frame_sequence);
        renderer.app.update();
        let update_ns = duration_ns(update_started.elapsed());
        if let Some(error) = renderer.load_error() {
            emit(AvatarEvent::Error { message: error });
            continue;
        }
        let initialized = renderer.ready(state.avatar_height(), state.height);
        if motion_state != Some(renderer.motion_state) {
            motion_state = Some(renderer.motion_state);
            emit(AvatarEvent::MotionState {
                state: renderer.motion_state,
            });
        }
        if backend != Backend::Shm
            && initialized
            && rendered_generation == renderer.generation
            && let Some(texture) = renderer.rendered_texture()
        {
            let mask_started = Instant::now();
            let runs = match backend {
                Backend::Geometry => Some(geometry::projected_runs(
                    renderer.app.world_mut(),
                    state.width * state.scale,
                    state.height * state.scale,
                    state.scale,
                )),
                Backend::Empty => Some(Vec::new()),
                _ => None,
            };
            let mask_ns = duration_ns(mask_started.elapsed());
            let total_ns = duration_ns(work_started.elapsed());
            if gpu_frames.as_mut().unwrap().enqueue(
                &texture,
                state.width * state.scale,
                state.height * state.scale,
                state.scale,
                renderer.generation,
                frame_sequence,
                runs,
            ) {
                cpu_frames.insert(
                    frame_sequence,
                    CpuFrameDurations {
                        frame_id: frame_sequence,
                        logical_width: state.width,
                        logical_height: state.height,
                        scale: state.scale as f64,
                        total_ns,
                        motion_ns: Some(motion_ns),
                        update_ns: Some(update_ns),
                        mask_ns: Some(mask_ns),
                        ..Default::default()
                    },
                );
                while cpu_frames.len() > 64 {
                    cpu_frames.pop_first();
                }
            }
        }
        let captured = renderer.captures.lock().unwrap();
        if backend == Backend::Shm
            && captured.revision != revision
            && initialized
            && captured.generation == renderer.generation
            && captured.width == state.width * state.scale
            && captured.height == state.height * state.scale
        {
            revision = captured.revision;
            let mask_started = Instant::now();
            let runs =
                mask::alpha_runs(&captured.rgba, captured.width, captured.height, state.scale);
            let mask_ns = duration_ns(mask_started.elapsed());
            let input_started = Instant::now();
            state.request_feedback(&qh, captured.revision);
            state.present(&captured.rgba, &runs)?;
            metrics.lock().unwrap().record_cpu_frame(CpuFrameDurations {
                frame_id: captured.revision,
                logical_width: state.width,
                logical_height: state.height,
                scale: state.scale as f64,
                total_ns: duration_ns(work_started.elapsed()),
                motion_ns: Some(motion_ns),
                update_ns: Some(update_ns),
                mask_ns: Some(mask_ns),
                input_region_ns: Some(duration_ns(input_started.elapsed())),
                ..Default::default()
            });
            metrics
                .lock()
                .unwrap()
                .record_gpu_durations(telemetry::GpuDurations {
                    frame_id: captured.revision,
                    readback_bytes: u64::from(captured.width) * u64::from(captured.height) * 4,
                    ..Default::default()
                });
            if initialized && !runs.is_empty() && !ready {
                emit(AvatarEvent::Ready {
                    model: renderer.credit.clone(),
                });
                ready = true;
            }
            if !runs.is_empty() && !presented {
                emit(AvatarEvent::VisibilityChanged { visible: true });
                presented = true;
            }
        }
        if !ready && load_started.elapsed() > Duration::from_secs(90) {
            renderer.model_failed = true;
            emit(AvatarEvent::Error {
                message:
                    "モデルの表示が90秒以内に完了しませんでした。設定でモデルを選び直してください。"
                        .into(),
            });
        }
    }
    drop(gpu_surface);
    drop(verifications);
    drop(gpu_frames);
    metrics.lock().unwrap().emit_summary();
    release_renderer(&mut renderer);
    emit(AvatarEvent::Stopped);
    Ok(())
}

fn release_renderer(renderer: &mut Option<renderer::Renderer>) {
    if let Some(mut renderer) = renderer.take() {
        renderer.drain_capture();
    }
}
