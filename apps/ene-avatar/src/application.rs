//! IPC, Wayland dispatch and render-loop coordination.
use crate::{
    ipc,
    render::{gpu, renderer},
    wayland as native,
};
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
use std::sync::Arc;

#[derive(clap::Parser)]
#[command(about = "Wayland layer-shell VRM renderer; normally controlled by ene-desktop")]
pub(super) struct Args {
    #[arg(long)]
    pub(super) standalone: bool,
    #[arg(long, required_if_eq("standalone", "true"))]
    model: Option<String>,
    #[arg(long)]
    motion: Option<String>,
    #[arg(long)]
    no_loop: bool,
}
pub(super) fn run(args: Args, ipc: &mut ipc::Ipc) -> Result<(), Box<dyn Error>> {
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
    let mut state = native::State::new(&globals, &qh, settings, connection.clone())?;
    queue.roundtrip(&mut state)?;
    state.recreate(&qh);
    let mut event_loop = EventLoop::<native::State>::try_new()?;
    WaylandSource::new(connection.clone(), queue).insert(event_loop.handle())?;
    let mut renderer = None::<renderer::Renderer>;
    let mut next_frame = Instant::now();
    let mut gpu_frames = None::<gpu::GpuFrames>;
    let mut gpu_surface = None::<gpu::GpuSurface>;
    let mut frame_sequence = 0_u64;

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
            gpu_frames = Some(gpu::GpuFrames::new(
                world.resource::<RenderDevice>().clone(),
                world.resource::<RenderQueue>().clone(),
            ));
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
            let device = renderer.app.world().resource::<RenderDevice>().clone();
            let queue_gpu = renderer.app.world().resource::<RenderQueue>().clone();
            let frames = gpu_frames.as_mut().unwrap();
            let mut completed = frames.poll();
            if let Some(error) = frames.take_errors().into_iter().next() {
                return Err(error.into());
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
                }
                let output = gpu_surface.as_mut().unwrap().draw(
                    &frame.texture,
                    frame.width,
                    frame.height,
                )?;
                if let Some(output) = output {
                    state.prepare_gpu_present(&frame.runs);
                    output.present();
                    revision = frame.sequence;
                    if !frame.runs.is_empty() && !ready {
                        emit(AvatarEvent::Ready {
                            model: renderer.credit.clone(),
                        });
                        ready = true;
                    }
                    if !frame.runs.is_empty() && !presented {
                        emit(AvatarEvent::VisibilityChanged { visible: true });
                        presented = true;
                    }
                }
            }
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
        renderer.update_motion(&state.settings);
        let rendered_generation = renderer.generation;
        renderer.app.update();
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
        if initialized
            && rendered_generation == renderer.generation
            && let Some(texture) = renderer.rendered_texture()
        {
            gpu_frames.as_mut().unwrap().enqueue(
                &texture,
                state.width * state.scale,
                state.height * state.scale,
                state.scale,
                renderer.generation,
                frame_sequence,
            );
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
    drop(gpu_frames);
    drop(renderer);
    emit(AvatarEvent::Stopped);
    Ok(())
}
