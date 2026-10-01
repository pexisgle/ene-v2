mod ipc;
mod mask;
mod model;
mod motion;
mod native;
mod pixels;
mod renderer;

use ene_shared::avatar::{AvatarEvent, AvatarRequest, AvatarSettings, MotionState};
use smithay_client_toolkit::{
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    shell::WaylandSurface,
};
use std::{
    error::Error,
    time::{Duration, Instant},
};
use wayland_client::{Connection, globals::registry_queue_init};

use ipc::emit;
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
    let mut state = native::State::new(&globals, &qh, settings)?;
    queue.roundtrip(&mut state)?;
    state.recreate(&qh);
    let mut event_loop = EventLoop::<native::State>::try_new()?;
    WaylandSource::new(connection.clone(), queue).insert(event_loop.handle())?;
    let mut renderer = None::<renderer::Renderer>;
    let mut last_frame = Instant::now();
    let mut motion_state = None::<MotionState>;
    let mut revision = 0;
    let mut ready = false;
    let mut load_started = Instant::now();
    let mut hidden = false;
    let mut presented = false;
    let mut resume_pending = false;
    loop {
        let wait = if !state.settings.visible
            || renderer
                .as_ref()
                .is_some_and(|renderer| renderer.model_failed)
        {
            Duration::from_millis(100)
        } else {
            Duration::from_secs_f64(1.0 / state.settings.fps as f64)
                .saturating_sub(last_frame.elapsed())
                .max(Duration::from_millis(1))
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
            )?);
            load_started = Instant::now();
        }
        let renderer = renderer.as_mut().unwrap();
        if renderer.model_failed {
            continue;
        }
        if state.resized {
            renderer.resize(state.width, state.height, state.scale);
            state.empty_input(&qh);
            state.resized = false;
        }
        if resume_pending {
            renderer.resume();
            load_started = Instant::now();
            resume_pending = false;
        }
        if last_frame.elapsed() < Duration::from_secs_f64(1.0 / state.settings.fps as f64) {
            continue;
        }
        last_frame = Instant::now();
        renderer.update_motion(&state.settings);
        renderer.capture();
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
        let captured = renderer.captures.lock().unwrap();
        if captured.revision != revision
            && initialized
            && captured.generation == renderer.generation
            && captured.width == state.width * state.scale
            && captured.height == state.height * state.scale
        {
            revision = captured.revision;
            let runs =
                mask::alpha_runs(&captured.rgba, captured.width, captured.height, state.scale);
            state.present(&captured.rgba, &runs)?;
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
    release_renderer(&mut renderer);
    emit(AvatarEvent::Stopped);
    Ok(())
}

fn release_renderer(renderer: &mut Option<renderer::Renderer>) {
    if let Some(mut renderer) = renderer.take() {
        renderer.drain_capture();
    }
}
