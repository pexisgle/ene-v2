#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod computer;
mod network;
#[cfg(target_os = "linux")]
mod observe_linux;
mod onboarding;
mod plugins;
mod provider_choices;
mod renderer;
mod settings;
mod sound;
mod tray;
mod window;
mod workspace;

use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use ene_rpc as w;
use gpui::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use network::{Network, id};
use renderer::Renderer;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "ene desktop companion")]
struct Args {
    #[arg(long, env = "ENE_HOST_CONNECTION")]
    connection: Option<PathBuf>,
    #[arg(long, env = "ENE_DESKTOP_RENDERER")]
    avatar: Option<PathBuf>,
    #[arg(long, env = "ENE_DESKTOP_ASSETS")]
    assets: Option<PathBuf>,
    #[arg(long)]
    plugin: Vec<PathBuf>,
    #[arg(long)]
    allow_input: bool,
    #[arg(long)]
    connect_only: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Local {
    id: String,
    eyes: bool,
    input: bool,
    speaker: bool,
    plugins: Vec<PathBuf>,
    workspace: Option<PathBuf>,
    avatar_scale: f32,
}

impl Default for Local {
    fn default() -> Self {
        Self {
            id: id(),
            eyes: false,
            input: false,
            speaker: true,
            plugins: Vec::new(),
            workspace: None,
            avatar_scale: 100.0,
        }
    }
}

impl Local {
    fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid desktop settings"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let local = Self::default();
                let mut staged = tempfile::NamedTempFile::new_in(
                    path.parent().context("desktop config path missing")?,
                )?;
                serde_json::to_writer(&mut staged, &local)?;
                staged.as_file().sync_all()?;
                match staged.persist_noclobber(path) {
                    Ok(_) => Ok(local),
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        serde_json::from_slice(&std::fs::read(path)?)
                            .context("invalid desktop settings")
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, path: &std::path::Path) -> Result<()> {
        let mut staged =
            tempfile::NamedTempFile::new_in(path.parent().context("desktop config path missing")?)?;
        serde_json::to_writer(&mut staged, self)?;
        staged.as_file().sync_all()?;
        staged.persist(path)?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let dirs = ProjectDirs::from("", "", "ene").context("cannot locate user directories")?;
    let config = dirs.config_local_dir().join("desktop-v2.json");
    std::fs::create_dir_all(config.parent().context("desktop config path missing")?)?;
    let mut local = Local::load(&config)?;
    let using_default = args.connection.is_none();
    let discovery = args
        .connection
        .or_else(|| {
            std::env::var_os("ENE_DEV_HOST_DIR")
                .map(|dir| PathBuf::from(dir).join("runtime/connection.json"))
        })
        .unwrap_or_else(|| {
            dirs.runtime_dir()
                .map_or_else(|| dirs.cache_dir().join("runtime"), PathBuf::from)
                .join("connection.json")
        });
    let exe = std::env::current_exe()?;
    let sibling = exe.parent().context("executable directory missing")?;
    let host_exe = sibling.join(format!("ene-host{}", std::env::consts::EXE_SUFFIX));
    let local_host =
        (using_default && !args.connect_only && host_exe.is_file()).then_some(host_exe);
    let assets = args.assets.unwrap_or_else(|| {
        let packaged = sibling.join("assets");
        if packaged.is_dir() {
            packaged
        } else {
            PathBuf::from("assets")
        }
    });
    let avatar = args
        .avatar
        .unwrap_or_else(|| sibling.join(format!("ene-avatar{}", std::env::consts::EXE_SUFFIX)));
    let stop = CancellationToken::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let (desktop_events, mut tray_actions) = mpsc::channel(16);
    let renderer = {
        let _runtime = runtime.enter();
        avatar
            .is_file()
            .then(|| {
                Renderer::spawn(
                    &avatar,
                    &config.with_file_name("avatar-layout.json"),
                    stop.clone(),
                    desktop_events.clone(),
                )
            })
            .transpose()
            .unwrap_or_else(|_| {
                tracing::warn!("avatar renderer unavailable");
                None
            })
    };
    local.plugins.extend(args.plugin);
    local.plugins.sort();
    local.plugins.dedup();
    let (plugins, plugin_notice) = match plugins::ClientPlugin::load(&local.plugins) {
        Ok(plugins) => (plugins, None),
        Err(_) => (
            Vec::new(),
            Some("端末プラグインを読み込めません。設定を確認してください。"),
        ),
    };
    let mut capabilities = Vec::new();
    if renderer.is_some() {
        let schema = r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#;
        for name in ["body.motion", "body.expression"] {
            capabilities.push(w::Capability {
                name: name.into(),
                schema_json: schema.into(),
                ..Default::default()
            });
        }
        capabilities.extend(plugins.iter().filter_map(plugins::ClientPlugin::capability));
    }
    local.input |= args.allow_input;
    if local.input && computer::available() {
        capabilities.push(computer::capability());
    }
    let workspace = workspace::Workspace::probe(local.workspace.as_deref());
    if let Some(workspace) = &workspace {
        capabilities.extend(workspace.specs());
    }
    let device = w::Device {
        id: local.id.clone(),
        label: "Desktop".into(),
        capabilities,
        eyes_enabled: local.eyes,
        ..Default::default()
    };
    let (actions, requests) = mpsc::channel(64);
    let (updates, incoming) = mpsc::channel(96);
    if let Some(notice) = plugin_notice {
        let _sent = updates.try_send(network::Update::Notice(notice.into()));
    }
    let managed_host = local_host.is_some();
    let network = Network {
        discovery,
        local_host,
        assets,
        device,
        renderer: renderer.clone(),
        plugins: plugins.into(),
        plugin_paths: local.plugins.clone(),
        stop: stop.clone(),
        speaker: local.speaker,
        avatar_scale: local.avatar_scale,
        workspace: workspace.map(std::sync::Arc::new),
    };
    runtime.spawn(network.run(requests, updates));
    let tray = tray::start(&runtime, stop.clone(), desktop_events);
    gpui::application().run(move |cx| {
        gpui::init(cx);
        let bounds = Bounds::centered(None, size(px(540.0), px(620.0)), cx);
        let result = gpui::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: true,
                ..Default::default()
            },
            cx,
            move |win, cx| {
                win.set_window_title("Ene");
                win.on_window_should_close(cx, move |win, _| {
                    if tray.load(std::sync::atomic::Ordering::Acquire) {
                        win.minimize_window();
                        return false;
                    }
                    true
                });
                cx.new(|cx| {
                    window::Window::new(
                        win,
                        cx,
                        window::WindowInit {
                            actions,
                            updates: incoming,
                            renderer,
                            config,
                            local,
                            managed_host,
                        },
                    )
                })
            },
        );
        let Ok((handle, content)) = result else {
            tracing::error!("desktop window could not be opened");
            cx.quit();
            return;
        };
        cx.spawn(async move |cx| {
            while let Some(action) = tray_actions.recv().await {
                let result = cx.update(|cx| {
                    handle.update(cx, |_, window, cx| {
                        content.update(cx, |content, cx| content.tray_action(action, window, cx));
                    })
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.activate(true);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
    stop.cancel();
    runtime.shutdown_timeout(std::time::Duration::from_secs(3));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_install_reuses_its_device_id_before_preferences_are_changed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("desktop-v2.json");
        let initial = Local::load(&path)?;
        let restarted = Local::load(&path)?;
        assert_eq!(initial.id, restarted.id);
        Ok(())
    }
}
