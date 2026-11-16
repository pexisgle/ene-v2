//! Starts the desktop client: one window, one session, one owner per resource.
//!
//! The session runs on its own thread with a single-threaded Tokio runtime,
//! because the audio streams and the avatar child are not `Send` on every
//! platform and must stay on one thread. The window is GPUI's, on the main
//! thread, and it reaches the session only through two channels. Window requests
//! are `Send` futures spawned on the same runtime, so there is exactly one
//! runtime in the process.

use ene_desktop::{Command, Connection, Desktop, Options, Session, bind_actions, focus_chat};
use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};
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
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1080.), px(760.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(480.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("ene".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| {
                cx.new(|cx| {
                    Desktop::new(
                        cx,
                        options(),
                        connection.clone(),
                        commands_tx.clone(),
                        updates_tx.clone(),
                        updates_rx,
                        handle.clone(),
                        shown.clone(),
                    )
                })
            },
        );
        match opened {
            Ok(window) => {
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
                cx.on_window_closed(|cx| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                cx.activate(true);
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
