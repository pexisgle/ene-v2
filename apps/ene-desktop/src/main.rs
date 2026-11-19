#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Starts the desktop client: one window, one session, one owner per resource.
//!
//! The session runs on its own thread with a single-threaded Tokio runtime,
//! because the audio streams and the avatar child are not `Send` on every
//! platform and must stay on one thread. The window is GPUI's, on the main
//! thread, and it reaches the session only through two channels. Window requests
//! are `Send` futures spawned on the same runtime, so there is exactly one
//! runtime in the process.

use ene_desktop::{
    Command, Connection, EmbeddedAssets, Hub, Options, PodMode, Session, Tray, TrayAction,
    TrayState, bind_actions, default_renderer, open_pod_window,
};
use gpui::{App, AppContext as _, Application};
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

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown panic payload"
        };
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown location".to_owned());
        tracing::error!(location = %location, payload = %payload, "ene-desktop panicked");
        eprintln!("ene-desktop panic at {location}: {payload}");
        default_hook(info);
    }));

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
    let (tray_tx, tray_rx) = unbounded_channel::<TrayAction>();
    let tray = Tray::spawn(TrayState::default(), tray_tx);
    let tray_handle = tray.as_ref().map(Tray::handle);
    let tray_rx = tray_handle.is_some().then_some(tray_rx);

    let shutdown_rx = {
        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();
        handle.spawn(async move {
            #[cfg(unix)]
            let received = {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                tokio::select! {
                    result = tokio::signal::ctrl_c() => result.is_ok(),
                    result = async {
                        match sigterm.as_mut() {
                            Some(stream) => stream.recv().await.is_some(),
                            None => std::future::pending::<bool>().await,
                        }
                    } => result,
                }
            };
            #[cfg(not(unix))]
            let received = tokio::signal::ctrl_c().await.is_ok();
            if received {
                let _ = sig_tx.send(());
            }
        });
        Some(sig_rx)
    };
    Application::new()
        .with_assets(EmbeddedAssets)
        .run(move |cx: &mut App| {
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
                    tray_handle,
                    tray_rx,
                    handle.clone(),
                    shown.clone(),
                )
            });
            let open_chat = std::env::var("ENE_DESKTOP_OPEN_SETTINGS")
                .ok()
                .is_some_and(|val| val.trim().eq_ignore_ascii_case("chat"));
            let open_section = std::env::var("ENE_DESKTOP_OPEN_SETTINGS")
                .ok()
                .and_then(|val| match val.trim().to_ascii_lowercase().as_str() {
                    "companions" => Some(ene_desktop::SettingsSection::Companions),
                    "connections" | "1" | "true" => Some(ene_desktop::SettingsSection::Connections),
                    "voice" => Some(ene_desktop::SettingsSection::Voice),
                    "device" => Some(ene_desktop::SettingsSection::Device),
                    _ => None,
                });
            if open_chat {
                hub.update(cx, |hub, _cx| hub.pod_mode = PodMode::ExpandedChat);
            }
            bind_actions(cx, hub.clone());
            if open_pod_window(cx, hub.clone()).is_none() {
                cx.quit();
                return;
            }
            let hub_closed = hub.clone();
            cx.on_window_closed(move |cx| {
                hub_closed.update(cx, |hub, cx| hub.sync_window_closed(cx));
            })
            .detach();
            cx.activate(true);
            if let Some(section) = open_section {
                hub.update(cx, |hub, cx| hub.open_settings(section, cx));
            }
        });

    // The window is gone. The session stops serving, kills its child and returns;
    // joining it is what makes the exit orderly rather than a race.
    drop(tray);
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
        renderer: std::env::var_os("ENE_DESKTOP_RENDERER")
            .map(PathBuf::from)
            .or_else(default_renderer),
        model: std::env::var_os("ENE_DESKTOP_MODEL")
            .map(PathBuf::from)
            .or(defaults.model),
    }
}
