mod mask;
mod model;
mod motion;
mod native;
mod pixels;
mod renderer;

use ene_shared::avatar::{AvatarEvent, AvatarRequest, AvatarSettings, MAX_IPC_LINE, MotionState};
use smithay_client_toolkit::{
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    shell::WaylandSurface,
};
use std::{
    error::Error,
    io::{BufRead, Read, Write},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wayland_client::{Connection, globals::registry_queue_init};

fn emit(event: AvatarEvent) {
    let mut output = std::io::stdout().lock();
    if serde_json::to_writer(&mut output, &event).is_ok() {
        let _ = output.write_all(b"\n");
        let _ = output.flush();
    }
}
fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let result = std::panic::catch_unwind(run);
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("ene-avatar: {error}");
            emit(AvatarEvent::Error {
                message: error.to_string(),
            });
            std::process::exit(1);
        }
        Err(_) => {
            emit(AvatarEvent::Error{message:"キャラクターの描画処理に失敗しました。GPU・モデルを確認して再起動してください。".into()});
            std::process::exit(1);
        }
    }
}
fn run() -> Result<(), Box<dyn Error>> {
    let mut settings = AvatarSettings::default();
    let mut standalone = false;
    let mut frames = None;
    let mut snapshot = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--standalone" => standalone = true,
            "--model" => settings.model = args.next().ok_or("--model にVRMのパスが必要です")?,
            "--motion" => {
                settings.motion = Some(args.next().ok_or("--motion にVRMAのパスが必要です")?)
            }
            "--no-loop" => settings.motion_loop = false,
            "--frames" => {
                frames = Some(
                    args.next()
                        .ok_or("--frames にフレーム数が必要です")?
                        .parse::<u64>()?,
                )
            }
            "--snapshot" => {
                snapshot = Some(PathBuf::from(
                    args.next().ok_or("--snapshot にパスが必要です")?,
                ))
            }
            "--help" | "-h" => {
                eprintln!(
                    "ene-avatar [--standalone --model /path/to/model.vrm] [--motion /path/to/motion.vrma] [--no-loop] [--frames N --snapshot output.png]\n通常はene-desktopからNDJSONで制御します。layer-shellが必要です。"
                );
                return Ok(());
            }
            _ => return Err(format!("未対応の引数: {arg}").into()),
        }
    }
    let (tx, rx) = mpsc::sync_channel(32);
    if !standalone {
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(std::io::stdin());
            loop {
                let mut bytes = Vec::new();
                match (&mut reader)
                    .take((MAX_IPC_LINE + 1) as u64)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(_) if bytes.len() > MAX_IPC_LINE => {
                        emit(AvatarEvent::Error {
                            message: "IPC要求が大きすぎます。".into(),
                        });
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(_) => match serde_json::from_slice(&bytes) {
                        Ok(request) => {
                            if tx.send(Some(request)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            emit(AvatarEvent::Error {
                                message: format!("IPC形式が不正です: {e}"),
                            });
                            let _ = tx.send(None);
                            break;
                        }
                    },
                }
            }
        });
        match rx.recv()? {
            Some(AvatarRequest::Configure { settings: s }) => settings = s,
            _ => return Ok(()),
        }
    }
    settings.validate()?;
    let mut credit = model::validate(&PathBuf::from(&settings.model))?;
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
    let mut frame_count = 0;
    let mut load_started = Instant::now();
    let mut hidden = false;
    loop {
        let wait = if !state.settings.visible {
            Duration::from_millis(100)
        } else {
            Duration::from_secs_f64(1.0 / state.settings.fps as f64)
                .saturating_sub(last_frame.elapsed())
                .max(Duration::from_millis(1))
        };
        event_loop.dispatch(wait, &mut state)?;
        connection.flush()?;
        let mut rebuild = state.output_changed;
        state.output_changed = false;
        let mut shutdown = false;
        while let Ok(request) = rx.try_recv() {
            match request {
                None | Some(AvatarRequest::Shutdown) => shutdown = true,
                Some(AvatarRequest::Configure { settings }) => {
                    settings.validate()?;
                    if state.settings.model != settings.model {
                        credit = model::validate(&PathBuf::from(&settings.model))?;
                        rebuild = true;
                    }
                    if state.settings.motion != settings.motion
                        || state.settings.motion_loop != settings.motion_loop
                    {
                        rebuild = true;
                    }
                    if state.settings.output != settings.output {
                        rebuild = true;
                    }
                    state.settings = settings;
                    state.properties();
                    state.layer.commit();
                }
            }
        }
        if shutdown {
            break;
        }
        if state.closed && !rebuild {
            return Err("キャラクターのWaylandウィンドウが閉じられました。".into());
        }
        if !state.settings.visible {
            if !hidden {
                release_renderer(&mut renderer);
                // Destroy the mapped role, including queued configure events,
                // rather than unmapping and acknowledging a reset serial.
                // The replacement layer has no buffer and an empty input region.
                state.recreate(&qh);
                hidden = true;
                emit(AvatarEvent::VisibilityChanged { visible: false });
            }
            continue;
        }
        if rebuild || hidden {
            release_renderer(&mut renderer);
            state.recreate(&qh);
            revision = 0;
            ready = false;
            hidden = false;
            load_started = Instant::now();
        }
        if !state.configured {
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
        if state.resized {
            renderer.resize(state.width, state.height, state.scale);
            state.empty_input(&qh);
            state.resized = false;
        }
        if last_frame.elapsed() < Duration::from_secs_f64(1.0 / state.settings.fps as f64) {
            continue;
        }
        last_frame = Instant::now();
        renderer.update_motion(&state.settings);
        renderer.capture();
        renderer.app.update();
        frame_count += 1;
        if let Some(error) = renderer.load_error() {
            return Err(error.into());
        }
        let initialized = renderer.ready();
        if motion_state != Some(renderer.motion_state) {
            motion_state = Some(renderer.motion_state);
            emit(AvatarEvent::MotionState {
                state: renderer.motion_state,
            });
        }
        if std::env::var_os("ENE_AVATAR_DEBUG").is_some() && frame_count % 30 == 0 {
            let world = renderer.app.world_mut();
            let meshes = world.query::<&bevy::prelude::Mesh3d>().iter(world).count();
            eprintln!(
                "frame={frame_count} initialized={initialized} meshes={meshes} load={:?}",
                renderer.load_state()
            );
        }
        let captured = renderer.captures.lock().unwrap();
        if captured.revision != revision
            && initialized
            && captured.generation == renderer.generation
            && captured.width == state.width * state.scale
            && captured.height == state.height * state.scale
        {
            revision = captured.revision;
            if std::env::var_os("ENE_AVATAR_DEBUG").is_some() {
                eprintln!(
                    "capture {} {}x{} alpha={}",
                    revision,
                    captured.width,
                    captured.height,
                    captured
                        .rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|p| p[3] > 8)
                        .count()
                );
            }
            let runs =
                mask::alpha_runs(&captured.rgba, captured.width, captured.height, state.scale);
            state.present(&captured.rgba, &runs)?;
            if initialized && !runs.is_empty() {
                if !ready {
                    emit(AvatarEvent::Ready {
                        model: credit.clone(),
                    });
                    emit(AvatarEvent::VisibilityChanged { visible: true });
                    ready = true;
                }
                if let Some(ref path) = snapshot {
                    let file = std::fs::File::create(path)?;
                    let mut encoder = png::Encoder::new(file, captured.width, captured.height);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    encoder.write_header()?.write_image_data(&captured.rgba)?;
                    snapshot = None;
                }
            }
        }
        if !ready && load_started.elapsed() > Duration::from_secs(90) {
            return Err(
                "モデルの表示が90秒以内に完了しませんでした。GPU・モデルを確認してください。"
                    .into(),
            );
        }
        if frames.is_some_and(|n| frame_count >= n) {
            if !ready || snapshot.is_some() {
                return Err("検証フレーム数以内にモデルを描画できませんでした。".into());
            }
            break;
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
