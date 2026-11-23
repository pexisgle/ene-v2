//! Interactive-desktop state and input injection.
//!
//! `Desktop` is the observation a capture was taken against: the foreground
//! window, its monitor, the virtual screen, and the last input tick. It is
//! opaque to callers, who compare two snapshots with [`Desktop::still_same`]
//! and read rects through accessors, so no `RECT` or `HWND` escapes.

#![cfg(windows)]

use windows::Win32::{
    Foundation::RECT,
    UI::{Input::KeyboardAndMouse as input, WindowsAndMessaging as window},
};

use crate::capture;

/// A snapshot of the interactive desktop a capture was taken against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Desktop {
    window: isize,
    window_bounds: RECT,
    monitor_rect: RECT,
    virtual_screen: RECT,
    last_input: u32,
}

impl Desktop {
    /// Reads the current desktop. Must run in the same physical-pixel DPI
    /// context as the capture, which `capture::physical` establishes.
    pub fn read() -> Option<Self> {
        if !capture::unlocked() {
            return None;
        }
        let foreground = capture::foreground_rect()?;
        let mut window_bounds = RECT::default();
        let mut activity = input::LASTINPUTINFO {
            cbSize: std::mem::size_of::<input::LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: the foreground handle came from a live query, and both calls
        // write into locals this function owns for the duration.
        unsafe {
            window::GetWindowRect(foreground.window_handle(), &mut window_bounds).ok()?;
            if !input::GetLastInputInfo(&mut activity).as_bool() {
                return None;
            }
            let left = window::GetSystemMetrics(window::SM_XVIRTUALSCREEN);
            let top = window::GetSystemMetrics(window::SM_YVIRTUALSCREEN);
            let width = window::GetSystemMetrics(window::SM_CXVIRTUALSCREEN);
            let height = window::GetSystemMetrics(window::SM_CYVIRTUALSCREEN);
            (width > 0 && height > 0).then_some(Self {
                window: foreground.window,
                window_bounds,
                monitor_rect: foreground.rect(),
                virtual_screen: RECT {
                    left,
                    top,
                    right: left + width,
                    bottom: top + height,
                },
                last_input: activity.dwTime,
            })
        }
    }

    /// Whether a later snapshot still describes the same desktop.
    pub fn still_matches(&self, later: &Self) -> bool {
        self == later
    }

    /// Builds a snapshot from plain geometry.
    ///
    /// Exists so callers and tests can describe a desktop without touching
    /// Win32; `read` is the only constructor that observes the real one.
    pub fn from_geometry(window: isize, monitor: Rect, virtual_screen: Rect) -> Self {
        let rect = |r: Rect| RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        Self {
            window,
            window_bounds: rect(monitor),
            monitor_rect: rect(monitor),
            virtual_screen: rect(virtual_screen),
            last_input: 0,
        }
    }

    /// The window this snapshot belongs to, as an opaque integer.
    pub fn window(&self) -> isize {
        self.window
    }

    /// The monitor rectangle the capture covers.
    pub fn monitor(&self) -> Rect {
        Rect::from(self.monitor_rect)
    }

    /// The whole virtual screen, for absolute pointer addressing.
    pub fn virtual_screen(&self) -> Rect {
        Rect::from(self.virtual_screen)
    }

    /// The last input tick Windows recorded, used to detect user activity.
    pub fn last_input(&self) -> u32 {
        self.last_input
    }
}

/// A screen rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<RECT> for Rect {
    fn from(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

/// Sends input events to the desktop as one batch.
///
/// Returns how many events the OS accepted; a value below `events.len()`
/// means the input was partially applied and the outcome is unknown.
pub fn send_input(events: &[input::INPUT]) -> usize {
    // SAFETY: `events` is a live slice and its length is passed in bytes as
    // `SendInput` requires; the OS copies the events before returning.
    unsafe { input::SendInput(events, std::mem::size_of::<input::INPUT>() as i32) as usize }
}

/// Why an input batch must not be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeToSend {
    /// A modifier or pointer button is held down.
    KeyHeld,
    /// The desktop changed since the capture was taken.
    DesktopChanged,
    /// The input target is owned by this process or the avatar.
    EneWindow,
    /// The target window could not be inspected.
    NoTarget,
}

impl UnsafeToSend {
    /// The message shown to the model for this refusal.
    pub fn message(self) -> &'static str {
        match self {
            Self::KeyHeld => {
                "a modifier or pointer button is held; take a new screenshot after releasing it"
            }
            Self::DesktopChanged => "the screen or user input changed; take a new screenshot",
            Self::EneWindow => "Computer Use cannot send input to ene windows",
            Self::NoTarget => "the input target is unavailable",
        }
    }
}

/// Checks that an input batch may be sent to the window a capture was taken
/// from, and that no key is held.
///
/// `target` is the point about to be clicked, when the action has one; the
/// window under it is rejected if it belongs to this process or the avatar.
pub fn check_before_send(
    captured: &Desktop,
    target: Option<(i32, i32)>,
    check_focus: bool,
    avatar_pid: Option<u32>,
) -> Result<(), UnsafeToSend> {
    let Some(now) = Desktop::read() else {
        return Err(UnsafeToSend::DesktopChanged);
    };
    if &now != captured {
        return Err(UnsafeToSend::DesktopChanged);
    }
    let mut gui = window::GUITHREADINFO {
        cbSize: std::mem::size_of::<window::GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: every call reads live OS state into locals this function owns;
    // the handles it returns are only inspected, never retained.
    unsafe {
        if [
            input::VK_SHIFT,
            input::VK_CONTROL,
            input::VK_MENU,
            input::VK_LWIN,
            input::VK_RWIN,
            input::VK_LBUTTON,
            input::VK_RBUTTON,
            input::VK_MBUTTON,
            input::VK_XBUTTON1,
            input::VK_XBUTTON2,
        ]
        .into_iter()
        .any(|key| input::GetAsyncKeyState(i32::from(key.0)) < 0)
        {
            return Err(UnsafeToSend::KeyHeld);
        }
        window::GetGUIThreadInfo(0, &mut gui).map_err(|_| UnsafeToSend::DesktopChanged)?;
        if gui.hwndActive.0 as isize != captured.window() || !gui.hwndCapture.is_invalid() {
            return Err(UnsafeToSend::DesktopChanged);
        }
        if !gui.hwndMenuOwner.is_invalid() {
            check_window(gui.hwndMenuOwner, avatar_pid)?;
        }
        if check_focus {
            check_window(gui.hwndActive, avatar_pid)?;
            if target.is_none() || !gui.hwndFocus.is_invalid() {
                check_window(gui.hwndFocus, avatar_pid)?;
            }
        }
        if let Some((x, y)) = target {
            let point = windows::Win32::Foundation::POINT { x, y };
            check_window(window::WindowFromPoint(point), avatar_pid)?;
        }
    }
    Ok(())
}

/// Rejects a window owned by this process or the avatar.
fn check_window(
    target: windows::Win32::Foundation::HWND,
    avatar_pid: Option<u32>,
) -> Result<(), UnsafeToSend> {
    if target.is_invalid() {
        return Err(UnsafeToSend::NoTarget);
    }
    // SAFETY: both handles came from live queries; `GetWindowThreadProcessId`
    // only writes to the stack local passed to it.
    unsafe {
        for hwnd in [target, window::GetAncestor(target, window::GA_ROOTOWNER)] {
            let mut pid = 0;
            if hwnd.is_invalid() || window::GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 {
                return Err(UnsafeToSend::NoTarget);
            }
            if pid == std::process::id() || Some(pid) == avatar_pid {
                return Err(UnsafeToSend::EneWindow);
            }
        }
    }
    Ok(())
}
