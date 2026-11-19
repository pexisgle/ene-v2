//! The desktop client: the process a person runs.
//!
//! It opens a window, connects to `ene-host`, declares what it can do, and serves
//! the companion the operator is talking to. The avatar renderer is its child,
//! and the audio devices and the window are its own.
//!
//! This is the reference client `docs/design/desktop-client.md` describes. It
//! owns no conversation, no memory and no model: those stay in the host.

mod audio;
mod avatar;
mod capabilities;
#[cfg(windows)]
mod computer;
mod host;
mod input;
mod perception;
mod session;
mod state;
mod tray;
mod ui;
mod update;
mod work_tools;

pub use avatar::{bundled_ene_model, default_model, default_renderer};
pub use host::{Connect, Discovery, Host};
pub use session::{Connection, Options, Session};
pub use state::{Link, Model};
pub use tray::{Tray, TrayAction, TrayHandle, TrayState};
pub use ui::{
    CompanionPod, Desktop, EmbeddedAssets, Hub, PodMode, SettingsSection, SettingsStudio,
    bind as bind_actions, focus_chat, initial_pod_bounds, initial_pod_bounds_for_mode,
    open_pod_window,
};
pub use update::{Command, Update};
pub use work_tools::WORKSPACE_FILE_CAPABILITIES;
