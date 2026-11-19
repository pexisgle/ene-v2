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

    let local_host = if cfg!(windows) && std::env::var_os("ENE_HOST_CONNECTION").is_none() {
        std::env::current_exe()
            .ok()
            .map(|exe| exe.with_file_name("ene-host.exe"))
            .filter(|host| host.is_file())
    } else {
        None
    };
    let path = match discovery_path(local_host.is_some()) {
        Ok(path) => path,
        Err(reason) => {
            // Without a runtime directory there is no host to find, so the
            // window could only repeat this sentence.
            eprintln!("ene-desktop: {reason}");
            std::process::exit(1);
        }
    };

    #[cfg(windows)]
    let local_host_child = local_host.as_deref().and_then(|host| {
        match local_host_command(host)
            .env("ENE_HOST_EXIT_ON_STDIN_EOF", "1")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => Some(child),
            Err(error) => {
                tracing::error!(%error, "could not start the local ene-host");
                None
            }
        }
    });
    #[cfg(windows)]
    let history_reset = local_host_child
        .as_ref()
        .map(|_| ene_desktop::HistoryReset {
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failure: std::env::var("ENE_DESKTOP_HISTORY_RESET_ERROR").ok(),
        });
    #[cfg(windows)]
    let history_reset_ui = history_reset.clone();
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
            #[cfg(windows)]
            if let Some(history_reset) = history_reset_ui {
                cx.set_global(history_reset);
            }
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
    #[cfg(windows)]
    if history_reset.is_some_and(|reset| reset.requested.load(std::sync::atomic::Ordering::Relaxed))
        && let (Some(host), Some(child)) = (local_host, local_host_child)
    {
        let reset_error = reset_local_history(&host, child).err();
        if let Err(error) = restart_desktop(reset_error) {
            tracing::error!(%error, "could not reopen ene after the history reset");
        }
    }
}

/// The discovery file: `ENE_HOST_CONNECTION`, or the host's default runtime
/// directory.
fn discovery_path(local_host: bool) -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ENE_HOST_CONNECTION") {
        return Ok(PathBuf::from(path));
    }
    if local_host && let Some(runtime) = std::env::var_os("ENE_HOST_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime).join("connection.json"));
    }
    let dirs = directories::ProjectDirs::from("", "", "ene")
        .ok_or("no runtime directory is configured; set ENE_HOST_CONNECTION")?;
    let runtime = dirs
        .runtime_dir()
        .map_or_else(|| dirs.cache_dir().join("runtime"), PathBuf::from);
    Ok(runtime.join("connection.json"))
}

#[cfg(windows)]
fn local_host_command(host: &std::path::Path) -> std::process::Command {
    use std::{os::windows::process::CommandExt as _, process::Command};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(host);
    if std::env::var_os("ENE_HOST_ALLOWED_CAPABILITIES").is_none() {
        let capabilities = ene_desktop::WORKSPACE_FILE_CAPABILITIES.join(",");
        command.env(
            "ENE_HOST_ALLOWED_CAPABILITIES",
            format!("body.motion,{capabilities},{}", ene_rpc::COMPUTER_USE),
        );
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
fn reset_local_history(
    host: &std::path::Path,
    mut child: std::process::Child,
) -> Result<(), String> {
    use std::time::{Duration, Instant};

    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    while child
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_none()
    {
        if Instant::now() >= deadline {
            return Err("ローカルホストの停止を待ってから、もう一度お試しください。".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let result = local_host_command(host)
        .arg("--reset-history")
        .output()
        .map_err(|error| error.to_string())?;
    if result.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&result.stderr).trim().to_owned())
    }
}

#[cfg(windows)]
fn restart_desktop(reset_error: Option<String>) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.env("ENE_DESKTOP_OPEN_SETTINGS", "device");
    if let Some(error) = reset_error {
        command.env("ENE_DESKTOP_HISTORY_RESET_ERROR", error);
    } else {
        command.env_remove("ENE_DESKTOP_HISTORY_RESET_ERROR");
    }
    command.spawn().map(|_| ())
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
