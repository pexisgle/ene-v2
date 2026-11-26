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
use parking_lot::RwLock;
use std::sync::{
    Arc,
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
    #[cfg(windows)]
    shared: Arc<RwLock<TrayState>>,
    #[cfg(windows)]
    events: winit::event_loop::EventLoopProxy<windows_tray::Event>,
    resident: Arc<AtomicBool>,
}

impl TrayHandle {
    pub fn is_resident(&self) -> bool {
        self.resident.load(Ordering::Acquire)
    }

    pub fn update(&self, next: TrayState) {
        #[cfg(windows)]
        {
            let mut current = self.shared.write();
            if *current != next {
                *current = next;
                drop(current);
                drop(self.events.send_event(windows_tray::Event::Refresh));
            }
        }
        #[cfg(not(windows))]
        drop(next);
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
        if let Some(thread) = self.thread.take() {
            drop(self.handle.events.send_event(windows_tray::Event::Quit));
            drop(thread.join());
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
mod menu;
#[cfg(windows)]
mod windows_tray;
