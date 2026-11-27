//! Command-line options shared by the window and session.
use clap::{Parser, ValueEnum};
use ene_desktop::{Options, SettingsSection, default_renderer};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "ene-desktop", version, about = "Run the ene desktop client")]
pub(super) struct DesktopArgs {
    /// Override the host discovery file.
    #[arg(long, env = "ENE_HOST_CONNECTION")]
    pub(super) connection: Option<std::ffi::OsString>,
    #[arg(env = "ENE_HOST_RUNTIME_DIR", long, hide = true)]
    host_runtime_dir: Option<std::ffi::OsString>,
    #[arg(env = "ENE_DEV_HOST_DIR", long, hide = true)]
    dev_host_dir: Option<std::ffi::OsString>,
    /// Start the host process next to the desktop binary.
    #[arg(long, overrides_with = "connect_only")]
    pub(super) local_host: bool,
    /// Connect to a separately managed host.
    #[arg(long, overrides_with = "local_host")]
    pub(super) connect_only: bool,
    /// Registered client ID.
    #[arg(long, env = "ENE_DESKTOP_CLIENT_ID")]
    client_id: Option<String>,
    /// Display name for this device.
    #[arg(long, env = "ENE_DESKTOP_CLIENT_NAME", default_value = "Desktop")]
    client_name: String,
    /// Path to the avatar renderer.
    #[arg(long, env = "ENE_DESKTOP_RENDERER")]
    renderer: Option<std::ffi::OsString>,
    /// Path to the avatar model.
    #[arg(long, env = "ENE_DESKTOP_MODEL")]
    model: Option<std::ffi::OsString>,
    /// The window or settings section to open at startup.
    #[arg(long, env = "ENE_DESKTOP_OPEN_SETTINGS", default_value = "pod", value_parser = OpenAt::parse)]
    pub(super) open_settings: OpenAt,
    #[cfg(windows)]
    #[arg(env = "ENE_DESKTOP_HISTORY_RESET_ERROR", long, hide = true)]
    pub(super) history_reset_error: Option<String>,
}

impl DesktopArgs {
    pub(super) fn options(&self) -> Options {
        Options {
            id: self
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map_or_else(ene_desktop::persistent_client_id, str::to_owned),
            name: self.client_name.clone(),
            platform: std::env::consts::OS.into(),
            explicit_connection: self
                .connection
                .as_ref()
                .is_some_and(|path| !path.is_empty()),
            renderer: self
                .renderer
                .as_deref()
                .filter(|path| !path.is_empty())
                .map_or_else(default_renderer, |path| Some(path.into())),
            model: self
                .model
                .as_deref()
                .map_or_else(ene_desktop::default_model, |path| Some(path.into())),
        }
    }

    pub(super) fn discovery_path(&self) -> Result<PathBuf, DesktopInitError> {
        if let Some(path) = self.connection.as_deref().filter(|path| !path.is_empty()) {
            return Ok(path.into());
        }
        if let Some(runtime) = self
            .host_runtime_dir
            .as_deref()
            .filter(|path| !path.is_empty())
        {
            return Ok(std::path::Path::new(runtime).join("connection.json"));
        }
        if let Some(dev) = self.dev_host_dir.as_deref().filter(|path| !path.is_empty()) {
            return Ok(std::path::Path::new(dev).join("runtime/connection.json"));
        }
        let dirs = directories::ProjectDirs::from("", "", "ene")
            .ok_or(DesktopInitError::MissingRuntimeDir)?;
        let runtime = dirs.runtime_dir().map_or_else(
            || dirs.cache_dir().join("runtime"),
            std::path::Path::to_path_buf,
        );
        Ok(runtime.join("connection.json"))
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum OpenAt {
    Pod,
    Chat,
    Companions,
    #[value(aliases = ["1", "true"])]
    Connections,
    Voice,
    Device,
}

impl OpenAt {
    fn parse(value: &str) -> Result<Self, clap::Error> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self::Pod);
        }
        Self::from_str(value, true)
            .map_err(|message| clap::Error::raw(clap::error::ErrorKind::InvalidValue, message))
    }

    pub(super) fn section(self) -> Option<SettingsSection> {
        Some(match self {
            Self::Companions => SettingsSection::Companions,
            Self::Connections => SettingsSection::Connections,
            Self::Voice => SettingsSection::Voice,
            Self::Device => SettingsSection::Device,
            Self::Pod | Self::Chat => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DesktopInitError {
    #[error("no runtime directory is configured; set ENE_HOST_CONNECTION")]
    MissingRuntimeDir,
    #[cfg(windows)]
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(windows)]
    #[error("ローカルホストの停止を待ってから、もう一度お試しください。")]
    HostStopTimeout,
    #[cfg(windows)]
    #[error("{0}")]
    ResetFailed(String),
}
