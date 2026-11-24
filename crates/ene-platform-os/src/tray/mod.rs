//! Taskbar notification area (system tray) residency for `ene-desktop`.
//!
//! Runs a dedicated message-loop thread on Windows so modal Win32 context menus
//! never block GPUI's UI thread, and reports user selections back over an
//! unbounded channel. On unsupported platforms `Tray::spawn` returns `None` so
//! the client degrades cleanly to standard window-lifetime shutdown.

/// Display mode of the frameless floating companion pod.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrayMode {
    #[default]
    MiniVoice,
    ExpandedChat,
}

/// A chapter of the settings window the tray can open directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TraySection {
    #[default]
    Companions,
    Connections,
    Voice,
    Device,
}
#[cfg(windows)]
use crate::best_effort::ignore_teardown;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::UnboundedSender;

/// Commands emitted from the taskbar notification area icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ActivatePod,
    OpenChat,
    OpenMiniPod,
    HidePod,
    OpenSettings(TraySection),
    CenterAvatar,
    ToggleMicMute,
    Quit,
}

/// Live client summary reflected in the tray tooltip and context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayState {
    pub pod_open: bool,
    pub pod_mode: TrayMode,
    pub mic_muted: bool,
    pub avatar_ready: bool,
    pub tooltip: String,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            pod_open: true,
            pod_mode: TrayMode::MiniVoice,
            mic_muted: false,
            avatar_ready: false,
            tooltip: "ene".to_owned(),
        }
    }
}

/// Cloneable UI-side handle to refresh tray state and check residency.
#[derive(Clone, Debug)]
pub struct TrayHandle {
    shared: Arc<RwLock<TrayState>>,
    #[cfg(windows)]
    hwnd_raw: isize,
    resident: Arc<AtomicBool>,
}

impl TrayHandle {
    pub fn is_resident(&self) -> bool {
        self.resident.load(Ordering::Acquire)
    }

    pub fn update(&self, next: TrayState) {
        let tip_changed = match self.shared.write() {
            Ok(mut guard) => {
                let changed = guard.tooltip != next.tooltip;
                *guard = next;
                changed
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                let changed = guard.tooltip != next.tooltip;
                *guard = next;
                changed
            }
        };
        #[cfg(windows)]
        if self.is_resident() && tip_changed {
            windows_tray::request_tip_refresh(self.hwnd_raw);
        }
        #[cfg(not(windows))]
        let _discarded = tip_changed;
    }
}

/// Owned tray lifecycle guard; closes the tray thread and removes the icon on exit.
#[derive(Debug)]
pub struct Tray {
    handle: TrayHandle,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Tray {
    pub fn spawn(initial: TrayState, tx: UnboundedSender<TrayAction>) -> Option<Self> {
        #[cfg(windows)]
        {
            windows_tray::spawn(initial, tx)
        }
        #[cfg(not(windows))]
        {
            let _discarded = (initial, tx);
            None
        }
    }

    pub fn handle(&self) -> TrayHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        #[cfg(windows)]
        {
            windows_tray::request_close(self.handle.hwnd_raw);
            if let Some(thread) = self.thread.take() {
                let _discarded = thread.join();
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
const CMD_OPEN_CHAT: u32 = 1;
#[cfg(windows)]
const CMD_OPEN_MINI: u32 = 2;
#[cfg(windows)]
const CMD_HIDE_POD: u32 = 3;
#[cfg(windows)]
const CMD_SETTINGS_COMPANIONS: u32 = 4;
#[cfg(windows)]
const CMD_SETTINGS_CONNECTIONS: u32 = 5;
#[cfg(windows)]
const CMD_SETTINGS_DEVICE: u32 = 6;
#[cfg(windows)]
const CMD_CENTER_AVATAR: u32 = 7;
#[cfg(windows)]
const CMD_TOGGLE_MIC: u32 = 8;
#[cfg(windows)]
const CMD_QUIT: u32 = 9;

#[cfg(windows)]
fn action_for_command(id: u32) -> Option<TrayAction> {
    match id {
        CMD_OPEN_CHAT => Some(TrayAction::OpenChat),
        CMD_OPEN_MINI => Some(TrayAction::OpenMiniPod),
        CMD_HIDE_POD => Some(TrayAction::HidePod),
        CMD_SETTINGS_COMPANIONS => Some(TrayAction::OpenSettings(TraySection::Companions)),
        CMD_SETTINGS_CONNECTIONS => Some(TrayAction::OpenSettings(TraySection::Connections)),
        CMD_SETTINGS_DEVICE => Some(TrayAction::OpenSettings(TraySection::Device)),
        CMD_CENTER_AVATAR => Some(TrayAction::CenterAvatar),
        CMD_TOGGLE_MIC => Some(TrayAction::ToggleMicMute),
        CMD_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

/// Encodes `text` as a NUL-terminated UTF-16 buffer fitting `NOTIFYICONDATAW::szTip`
/// (`[u16; 128]`), stopping cleanly before splitting a surrogate pair.
#[cfg(windows)]
fn encode_tooltip(text: &str) -> [u16; 128] {
    let mut buf = [0u16; 128];
    let limit = buf.len() - 1;
    let mut len = 0usize;
    for ch in text.chars() {
        let mut units = [0u16; 2];
        let encoded = ch.encode_utf16(&mut units);
        if len + encoded.len() > limit {
            break;
        }
        buf[len..len + encoded.len()].copy_from_slice(encoded);
        len += encoded.len();
    }
    buf
}

#[cfg(windows)]
mod windows_tray;
