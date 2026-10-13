//! IPC, Wayland dispatch and render-loop coordination.
use crate::{
    ipc,
    render::{gpu, renderer},
    wayland as native,
};
use ene_ipc::renderer::{
    AvatarConfig, AvatarEvent, AvatarId, AvatarInstance, AvatarPlacement, MotionState,
};
#[cfg(unix)]
use smithay_client_toolkit::{
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    shell::WaylandSurface,
};
use std::{
    error::Error,
    time::{Duration, Instant},
};
#[cfg(unix)]
use wayland_client::{Connection, globals::registry_queue_init};

use bevy::render::renderer::{RenderAdapter, RenderDevice, RenderInstance, RenderQueue};
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
impl Args {
    pub(super) fn initial_avatar(self) -> Option<AvatarInstance> {
        self.standalone.then(|| AvatarInstance {
            id: AvatarId(1),
            config: AvatarConfig {
                model: self.model.unwrap_or_default(),
                motion: self.motion,
                motion_loop: !self.no_loop,
                ..Default::default()
            },
            placement: AvatarPlacement::default(),
        })
    }
}

pub(crate) fn run_window(
    id: AvatarId,
    settings: AvatarConfig,
    placement: AvatarPlacement,
    mut fps: u32,
    ipc: ipc::WindowIpc,
) -> Result<(), Box<dyn Error>> {
    let ipc::WindowIpc { requests, output } = ipc;
    #[cfg(unix)]
    let (mut state, mut event_loop, connection, qh) = {
        let connection = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init(&connection)?;
        let qh = queue.handle();
        let mut state = native::State::new(
            &globals,
            &qh,
            id,
            settings,
            placement,
            connection.clone(),
            output.clone(),
        )?;
        queue.roundtrip(&mut state)?;
        state.recreate(&qh);
        let event_loop = EventLoop::<native::State>::try_new()?;
        WaylandSource::new(connection.clone(), queue).insert(event_loop.handle())?;
        event_loop
            .handle()
            .insert_source(requests, |event, _, state| match event {
                smithay_client_toolkit::reexports::calloop::channel::Event::Msg(request) => {
                    state.requests.push(request)
                }
                smithay_client_toolkit::reexports::calloop::channel::Event::Closed => {
                    state.stopping = true
                }
            })?;
        (state, event_loop, connection, qh)
    };
    #[cfg(windows)]
    let mut state = native::State::new(id, settings, placement, output.clone())?;
    output.update_state(id, |status| status.placement = state.placement.clone());
    let mut renderer = None::<renderer::Renderer>;
    let mut next_frame = Instant::now();
    let mut gpu_frames = None::<gpu::GpuFrames>;
    let mut gpu_surface = None::<gpu::AvatarPresenter>;
    let mut frame_sequence = 0_u64;

    let mut motion_state = None::<MotionState>;
    let mut ready = false;
    let mut load_started = Instant::now();
    let mut resume_pending = false;
    loop {
        let wait = if !state.configured
            || renderer
                .as_ref()
                .is_some_and(|renderer| renderer.model_failed)
        {
            None
        } else {
            Some(
                next_frame
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(2)),
            )
        };
        #[cfg(unix)]
        {
            event_loop.dispatch(wait, &mut state)?;
            state.apply_drag();
            connection.flush()?;
        }
        #[cfg(windows)]
        {
            state.dispatch(
                wait.unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20)),
            );
            loop {
                match requests.try_recv() {
                    Ok(request) => state.requests.push(request),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        state.stopping = true;
                        break;
                    }
                }
            }
        }
        let remap = state.output_changed || state.closed;
        state.output_changed = false;
        let mut configure_renderer = false;
        let mut properties_changed = false;
        if state.stopping {
            break;
        }
        for request in std::mem::take(&mut state.requests) {
            match request {
                ipc::WindowRequest::Recenter => state.request_recenter(),
                ipc::WindowRequest::RenderConfig(config) => {
                    fps = config.fps;
                    next_frame = Instant::now();
                }
                ipc::WindowRequest::Configure(settings) => {
                    if state.settings != settings {
                        let model_changed = state.settings.model != settings.model;
                        let motion_changed = state.settings.motion != settings.motion;
                        properties_changed |= state.settings.size_percent != settings.size_percent;
                        output.update_state(id, |status| {
                            if model_changed {
                                status.loading = true;
                                status.error = None;
                            }
                            if motion_changed || model_changed {
                                status.motion = MotionState::None;
                            }
                        });
                        state.settings = settings;
                        configure_renderer |= model_changed || motion_changed;
                    }
                }
            }
        }

        // Coalesce settings to the last request. Model/motion changes update
        // the existing asset server and GPU instead of rebuilding Bevy.
        if configure_renderer && let Some(renderer) = renderer.as_mut() {
            match renderer.configure(&state.settings) {
                Ok(true) => {
                    ready = false;
                    load_started = Instant::now();
                    resume_pending = true;
                }
                Ok(false) if ready => {
                    // Selecting the still-active model after a bad choice
                    // also clears the error without reloading or restarting.
                    output.update_state(id, |status| {
                        status.model = Some(renderer.credit.clone());
                        status.loading = false;
                        status.error = None;
                    });
                }
                Ok(false) => {}
                Err(message) => output.avatar_error(id, message),
            }
        }
        if properties_changed && !remap {
            state.properties();
            #[cfg(unix)]
            state.layer.commit();
        }
        if remap {
            // Layer-shell fixes the output at creation. Replace only this role;
            // the renderer, model and animation remain alive.
            gpu_surface = None;
            #[cfg(unix)]
            state.recreate(&qh);
            #[cfg(windows)]
            state.recreate();
            if let Some(renderer) = renderer.as_mut() {
                renderer.generation = renderer
                    .generation
                    .checked_add(1)
                    .ok_or("avatar generation exhausted")?;
            }
            load_started = Instant::now();
            resume_pending = true;
        }
        state.apply_recenter();
        if !state.configured {
            resume_pending = true;
            continue;
        }
        if renderer.is_none() {
            output.update_state(id, |status| status.loading = true);
            renderer = Some(renderer::Renderer::new(
                state.width,
                state.height,
                state.scale,
                &state.settings,
                state.native_window(),
                id,
                &output,
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
            let completed = frames.poll();
            if let Some(error) = frames.take_errors().into_iter().next() {
                return Err(error.into());
            }
            if !completed.is_empty() {
                if gpu_surface.is_none() {
                    let world = renderer.app.sub_app(bevy::render::RenderApp).world();
                    gpu_surface = Some(gpu::AvatarPresenter::new(
                        world.resource::<RenderInstance>(),
                        world.resource::<RenderAdapter>(),
                        device.wgpu_device(),
                        &queue_gpu,
                        Arc::new(state.native_window()),
                    )?);
                }
                if let Some(covered) = gpu_surface.as_mut().unwrap().present_latest(
                    completed,
                    &mut state,
                    renderer.generation,
                )? && covered
                    && !ready
                {
                    output.update_state(id, |status| {
                        status.model = Some(renderer.credit.clone());
                        status.loading = false;
                        status.error = None;
                    });
                    ready = true;
                }
            }
        }
        if Instant::now() < next_frame {
            continue;
        }
        let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        if fps == 0 {
            next_frame = Instant::now();
        } else {
            next_frame += interval;
        }
        if fps != 0 && next_frame < Instant::now() {
            next_frame = Instant::now() + interval;
        }
        frame_sequence = frame_sequence
            .checked_add(1)
            .ok_or("avatar sequence exhausted")?;
        renderer.update_motion(&state.settings);
        let rendered_generation = renderer.generation;
        renderer.app.update();
        if let Some(error) = renderer.load_error() {
            output.avatar_error(id, error);
            continue;
        }
        let initialized = renderer.ready(state.avatar_height(), state.height);
        if motion_state != Some(renderer.motion_state.clone()) {
            motion_state = Some(renderer.motion_state.clone());
            output.update_state(id, |status| status.motion = renderer.motion_state.clone());
            if renderer.motion_state == MotionState::Finished {
                output.emit(id, AvatarEvent::MotionFinished);
            }
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
            output.avatar_error(
                id,
                "モデルの表示が90秒以内に完了しませんでした。設定でモデルを選び直してください。",
            );
        }
    }
    drop(gpu_surface);
    drop(gpu_frames);
    drop(renderer);
    Ok(())
}
