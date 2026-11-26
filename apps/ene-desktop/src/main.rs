// The desktop app performs no FFI itself: every platform call lives in
// ne-platform-os, which states the invariant each one relies on.
#![forbid(unsafe_code)]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Starts the desktop client: one window, one session, one owner per resource.
//!
//! The session runs on its own thread with a single-threaded Tokio runtime,
//! because the audio streams and the avatar child are not `Send` on every
//! platform and must stay on one thread. The window is GPUI's, on the main
//! thread, and it reaches the session only through two channels. Window requests
//! are `Send` futures spawned on the same runtime, so there is exactly one
//! runtime in the process.

mod args;

use args::{DesktopArgs, OpenAt};
use clap::Parser as _;
use ene_desktop::{
    Command, Connection, EmbeddedAssets, Hub, Options, PodMode, Session, Tray, TrayAction,
    TrayState, bind_actions, open_pod_window,
};
use gpui::{App, AppContext as _};
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

struct SessionRuntime {
    handle: tokio::runtime::Handle,
    worker: std::thread::JoinHandle<()>,
}

struct DesktopAppInit {
    connection: Connection,
    options: Options,
    open_at: OpenAt,
    commands_tx: UnboundedSender<Command>,
    updates_tx: UnboundedSender<ene_desktop::Update>,
    updates_rx: UnboundedReceiver<ene_desktop::Update>,
    tray_handle: Option<ene_desktop::TrayHandle>,
    tray_rx: Option<UnboundedReceiver<TrayAction>>,
    handle: tokio::runtime::Handle,
    shown: String,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    #[cfg(windows)]
    history_reset_ui: Option<ene_desktop::HistoryReset>,
}

fn main() {
    let args = DesktopArgs::parse();
    init_tracing();
    install_panic_hook();

    let local_host = detect_local_host(&args);
    let path = args.discovery_path().unwrap_or_else(|reason| {
        // Without a runtime directory there is no host to find, so the
        // window could only repeat this sentence.
        tracing::error!(%reason, "could not resolve host discovery path");
        std::process::exit(1);
    });

    let local_host_child = spawn_local_host(local_host.as_deref());
    #[cfg(windows)]
    let history_reset = local_host_child
        .as_ref()
        .map(|_| ene_desktop::HistoryReset {
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failure: args.history_reset_error.clone(),
        });

    let (updates_tx, updates_rx) = unbounded_channel();
    let (commands_tx, commands_rx) = unbounded_channel();
    let connection: Connection = Arc::new(RwLock::new(None));
    let options = args.options();
    let session_runtime = start_session_runtime(
        path.clone(),
        options.clone(),
        connection.clone(),
        updates_tx.clone(),
        commands_rx,
    );

    let quit = commands_tx.clone();
    let (tray_tx, tray_rx) = unbounded_channel::<TrayAction>();
    let tray = Tray::spawn(TrayState::default(), tray_tx);
    let tray_handle = tray.as_ref().map(Tray::handle);
    let tray_rx = tray_handle.is_some().then_some(tray_rx);
    let shutdown_rx = spawn_shutdown_signal(&session_runtime.handle);

    run_gpui_app(DesktopAppInit {
        connection,
        options,
        open_at: args.open_settings,
        commands_tx,
        updates_tx,
        updates_rx,
        tray_handle,
        tray_rx,
        handle: session_runtime.handle.clone(),
        shown: path.display().to_string(),
        shutdown_rx,
        #[cfg(windows)]
        history_reset_ui: history_reset.clone(),
    });

    // The window is gone. The session stops serving, kills its child and returns;
    // joining it is what makes the exit orderly rather than a race.
    drop(tray);
    ene_domain::debug_if_err(
        quit.send(Command::Quit)
            .map_err(|_| "the session already stopped"),
        "quit command",
    );
    ene_domain::warn_if_err(
        session_runtime
            .worker
            .join()
            .map_err(|_| "the worker panicked"),
        "session worker shutdown",
    );
    #[cfg(windows)]
    maybe_finish_history_reset(history_reset, local_host, local_host_child);
    #[cfg(not(windows))]
    if let Some(mut host) = local_host_child {
        ene_domain::warn_if_err(host.wait(), "local ene-host shutdown");
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown panic payload"
        };
        let location = info.location().map_or_else(
            || "unknown location".to_owned(),
            |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        );
        tracing::error!(location = %location, payload = %payload, "ene-desktop panicked");
        // architecture:allow-stderr: panic fallback stderr output if the tracing subscriber is unavailable
        eprintln!("ene-desktop panic at {location}: {payload}");
        default_hook(info);
    }));
}

fn detect_local_host(args: &DesktopArgs) -> Option<PathBuf> {
    if args
        .connection
        .as_ref()
        .is_some_and(|path| !path.is_empty())
        || args.connect_only
        || !(args.local_host || cfg!(windows))
    {
        return None;
    }
    let host = std::env::current_exe()
        .ok()?
        .with_file_name(format!("ene-host{}", std::env::consts::EXE_SUFFIX));
    host.is_file().then_some(host)
}

fn spawn_local_host(local_host: Option<&std::path::Path>) -> Option<std::process::Child> {
    local_host.and_then(|host| {
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
    })
}

fn start_session_runtime(
    path: PathBuf,
    options: Options,
    connection: Connection,
    updates_tx: UnboundedSender<ene_desktop::Update>,
    commands_rx: UnboundedReceiver<Command>,
) -> SessionRuntime {
    let (ready, handle) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("ene-session".into())
        .spawn(move || run_session(path, options, connection, updates_tx, commands_rx, &ready))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "the session thread did not start");
            std::process::exit(1);
        });
    let handle = handle.recv().unwrap_or_else(|error| {
        tracing::error!(%error, "the session runtime did not start");
        std::process::exit(1);
    });
    SessionRuntime { handle, worker }
}

// Starts the async session on its owner thread while GPUI owns the main thread.
#[tokio::main(flavor = "current_thread")]
async fn run_session(
    path: PathBuf,
    options: Options,
    connection: Connection,
    updates: UnboundedSender<ene_desktop::Update>,
    commands: UnboundedReceiver<Command>,
    ready: &std::sync::mpsc::SyncSender<tokio::runtime::Handle>,
) {
    if ready.send(tokio::runtime::Handle::current()).is_ok() {
        let session = Session::new(path, options, connection, updates, commands).await;
        Box::pin(session.run()).await;
    }
}

fn spawn_shutdown_signal(handle: &tokio::runtime::Handle) -> tokio::sync::oneshot::Receiver<()> {
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
            ene_domain::debug_if_err(
                sig_tx.send(()).map_err(|_| "the listener stopped"),
                "shutdown signal",
            );
        }
    });
    sig_rx
}

fn run_gpui_app(init: DesktopAppInit) {
    gpui::application()
        .with_assets(EmbeddedAssets)
        .run(move |cx: &mut App| {
            #[cfg(windows)]
            if let Some(history_reset) = init.history_reset_ui {
                cx.set_global(history_reset);
            }
            let shutdown_rx = init.shutdown_rx;
            cx.spawn(async move |cx| {
                if shutdown_rx.await.is_ok() {
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();
            let hub = cx.new(|cx| {
                Hub::new(
                    cx,
                    ene_desktop::HubInit {
                        options: init.options,
                        connection: init.connection.clone(),
                        commands: init.commands_tx.clone(),
                        updates_tx: init.updates_tx.clone(),
                        updates: init.updates_rx,
                        tray: init.tray_handle,
                        tray_rx: init.tray_rx,
                        runtime: init.handle.clone(),
                        discovery: init.shown.clone(),
                    },
                )
            });
            if matches!(init.open_at, OpenAt::Chat) {
                hub.update(cx, |hub, _cx| hub.windows.pod_mode = PodMode::ExpandedChat);
            }
            bind_actions(cx, hub.clone());
            if open_pod_window(cx, &hub).is_none() {
                cx.quit();
                return;
            }
            let hub_closed = hub.clone();
            cx.on_window_closed(move |cx, _| {
                hub_closed.update(cx, |hub, cx| hub.sync_window_closed(cx));
            })
            .detach();
            cx.activate(true);
            if let Some(section) = init.open_at.section() {
                hub.update(cx, |hub, cx| hub.open_settings(section, cx));
            }
        });
}

#[cfg(windows)]
fn maybe_finish_history_reset(
    history_reset: Option<ene_desktop::HistoryReset>,
    local_host: Option<PathBuf>,
    local_host_child: Option<std::process::Child>,
) {
    if history_reset.is_some_and(|reset| reset.requested.load(std::sync::atomic::Ordering::Relaxed))
        && let (Some(host), Some(child)) = (local_host, local_host_child)
    {
        let reset_error = reset_local_history(&host, child)
            .err()
            .map(|error| error.to_string());
        if let Err(error) = restart_desktop(reset_error) {
            tracing::error!(%error, "could not reopen ene after the history reset");
        }
    }
}

fn local_host_command(host: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(host);
    if std::env::var_os("ENE_HOST_ALLOWED_CAPABILITIES").is_none() {
        command.env(
            "ENE_HOST_ALLOWED_CAPABILITIES",
            ene_desktop::default_allowed_capabilities(),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        // Let the desktop close the host's stdin on Ctrl-C.
        command.process_group(0);
    }
    command
}

#[cfg(windows)]
fn reset_local_history(
    host: &std::path::Path,
    mut child: std::process::Child,
) -> Result<(), DesktopInitError> {
    use std::time::{Duration, Instant};

    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            return Err(DesktopInitError::HostStopTimeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let result = local_host_command(host).arg("--reset-history").output()?;
    if result.status.success() {
        Ok(())
    } else {
        Err(DesktopInitError::ResetFailed(
            String::from_utf8_lossy(&result.stderr).trim().to_owned(),
        ))
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
