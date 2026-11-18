//! Starts the desktop client: one window, one session, one owner per resource.
//!
//! The session runs on its own thread with a single-threaded Tokio runtime,
//! because the audio streams and the avatar child are not `Send` on every
//! platform and must stay on one thread. The window is GPUI's, on the main
//! thread, and it reaches the session only through two channels. Window requests
//! are `Send` futures spawned on the same runtime, so there is exactly one
//! runtime in the process.

use ene_desktop::{
    Command, CompanionPod, Connection, EmbeddedAssets, Hub, Options, PodMode, Session,
    bind_actions, focus_chat, initial_pod_bounds,
};
use gpui::{AnyWindowHandle, App, AppContext as _, Application, WindowBounds, WindowOptions};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let path = match discovery_path() {
        Ok(path) => path,
        Err(reason) => {
            // Without a runtime directory there is no host to find, so the
            // window could only repeat this sentence.
            eprintln!("ene-desktop: {reason}");
            std::process::exit(1);
        }
    };
    let (updates_tx, updates_rx) = unbounded_channel();
    let (commands_tx, commands_rx) = unbounded_channel();
    let connection: Connection = Arc::new(RwLock::new(None));

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ene-desktop: the async runtime did not start: {error}");
            std::process::exit(1);
        }
    };
    let handle = runtime.handle().clone();
    let session = Session::new(
        path.clone(),
        options(),
        connection.clone(),
        updates_tx.clone(),
        commands_rx,
    );
    let worker = std::thread::Builder::new()
        .name("ene-session".into())
        .spawn(move || {
            runtime.block_on(session.run());
        })
        .expect("the session thread starts");

    let quit = commands_tx.clone();
    let shown = path.display().to_string();
    #[cfg(unix)]
    let shutdown_rx = {
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();
        handle.spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    match sigterm.as_mut() {
                        Some(stream) => {
                            stream.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
            let _ = sig_tx.send(());
        });
        Some(sig_rx)
    };
    Application::new()
        .with_assets(EmbeddedAssets)
        .run(move |cx: &mut App| {
            #[cfg(unix)]
            if let Some(rx) = shutdown_rx {
                cx.spawn(async move |cx| {
                    if rx.await.is_ok() {
                        let _ = cx.update(|cx| cx.quit());
                    }
                })
                .detach();
            }
            let hub = cx.new(|cx| {
                Hub::new(
                    cx,
                    options(),
                    connection.clone(),
                    commands_tx.clone(),
                    updates_tx.clone(),
                    updates_rx,
                    handle.clone(),
                    shown.clone(),
                )
            });
            let bounds = initial_pod_bounds(cx);
            let open_section = std::env::var("ENE_DESKTOP_OPEN_SETTINGS")
                .ok()
                .and_then(|val| match val.trim().to_ascii_lowercase().as_str() {
                    "companions" => Some(ene_desktop::SettingsSection::Companions),
                    "connections" | "1" | "true" => Some(ene_desktop::SettingsSection::Connections),
                    "device" => Some(ene_desktop::SettingsSection::Device),
                    _ => None,
                });
            let hub_for_pod = hub.clone();
            let opened = cx.open_window(
                WindowOptions {
                    #[cfg(windows)]
                    kind: gpui::WindowKind::PopUp,
                    #[cfg(not(windows))]
                    kind: gpui::WindowKind::Floating,
                    window_background: gpui::WindowBackgroundAppearance::Transparent,
                    window_decorations: Some(gpui::WindowDecorations::Client),
                    app_id: Some("ene-desktop".into()),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(PodMode::MiniVoice.min_size()),
                    is_movable: true,
                    is_resizable: false,
                    titlebar: None,
                    ..Default::default()
                },
                move |_window, cx| cx.new(|cx| CompanionPod::new(hub_for_pod.clone(), cx)),
            );
            match opened {
                Ok(window) => {
                    let pod_any: AnyWindowHandle = window.into();
                    let view = window.update(cx, |view, window, cx| {
                        focus_chat(view, window, cx);
                        cx.entity().downgrade()
                    });
                    match view {
                        Ok(view) => bind_actions(cx, view),
                        Err(error) => {
                            eprintln!("ene-desktop: the window content is unavailable: {error:#}");
                        }
                    }
                    cx.on_window_closed(move |cx| {
                        if cx.windows().is_empty() || !cx.windows().contains(&pod_any) {
                            cx.quit();
                        }
                    })
                    .detach();
                    cx.activate(true);
                    if let Some(section) = open_section {
                        hub.update(cx, |hub, cx| hub.open_settings(section, cx));
                    }
                }
                Err(error) => {
                    eprintln!("ene-desktop: the window did not open: {error:#}");
                    cx.quit();
                }
            }
        });

    // The window is gone. The session stops serving, kills its child and returns;
    // joining it is what makes the exit orderly rather than a race.
    let _ = quit.send(Command::Quit);
    let _ = worker.join();
}

/// The discovery file: `ENE_HOST_CONNECTION`, or the host's default runtime
/// directory.
fn discovery_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ENE_HOST_CONNECTION") {
        return Ok(PathBuf::from(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "ene")
        .ok_or("no runtime directory is configured; set ENE_HOST_CONNECTION")?;
    let runtime = dirs
        .runtime_dir()
        .map_or_else(|| dirs.cache_dir().join("runtime"), PathBuf::from);
    Ok(runtime.join("connection.json"))
}

/// The client's identity and renderer, from the environment.
fn options() -> Options {
    let defaults = Options::default();
    Options {
        id: std::env::var("ENE_DESKTOP_CLIENT_ID").unwrap_or(defaults.id),
        name: std::env::var("ENE_DESKTOP_CLIENT_NAME").unwrap_or(defaults.name),
        platform: defaults.platform,
        renderer: std::env::var_os("ENE_DESKTOP_RENDERER").map(PathBuf::from),
        model: std::env::var_os("ENE_DESKTOP_MODEL")
            .map(PathBuf::from)
            .or(defaults.model),
    }
}
