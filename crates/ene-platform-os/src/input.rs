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

mod actions;
pub use actions::{Button, Input, InputError, Key, Point, apply_input};

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
        // SAFETY: the handle came from a live foreground query and `window_bounds`
        // is a local this function owns.
        unsafe { window::GetWindowRect(foreground.window_handle(), &mut window_bounds).ok()? };
        // SAFETY: `activity` is a live `LASTINPUTINFO` whose `cbSize` is set.
        if !unsafe { input::GetLastInputInfo(&mut activity) }.as_bool() {
            return None;
        }
        // SAFETY: each call reads one virtual-screen metric; no pointer involved.
        let left = unsafe { window::GetSystemMetrics(window::SM_XVIRTUALSCREEN) };
        // SAFETY: as above, for the top edge.
        let top = unsafe { window::GetSystemMetrics(window::SM_YVIRTUALSCREEN) };
        // SAFETY: as above, for the width.
        let width = unsafe { window::GetSystemMetrics(window::SM_CXVIRTUALSCREEN) };
        // SAFETY: as above, for the height.
        let height = unsafe { window::GetSystemMetrics(window::SM_CYVIRTUALSCREEN) };
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
fn send_input(events: &[input::INPUT]) -> usize {
    // SAFETY: `events` is a live slice and its length is passed in bytes as
    // `SendInput` requires; the OS copies the events before returning.
    unsafe { input::SendInput(events, std::mem::size_of::<input::INPUT>() as i32) as usize }
}

/// Why an input batch must not be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnsafeToSend {
    #[error("a modifier or pointer button is held; take a new screenshot after releasing it")]
    KeyHeld,
    #[error("the screen or user input changed; take a new screenshot")]
    DesktopChanged,
    #[error("Computer Use cannot send input to ene windows")]
    EneWindow,
    #[error("the input target is unavailable")]
    NoTarget,
}

/// Checks that an input batch may be sent to the window a capture was taken
/// from, and that no key is held.
///
/// `target` is the point about to be clicked, when the action has one; the
/// window under it is rejected if it belongs to this process or the avatar.
fn check_before_send(
    captured: &Desktop,
    target: Option<Point>,
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
    // SAFETY: reads the current async state of each key; takes an integer and
    // returns an integer, so nothing is borrowed or retained.
    let held = [
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
    .any(|key| unsafe { input::GetAsyncKeyState(i32::from(key.0)) } < 0);
    if held {
        return Err(UnsafeToSend::KeyHeld);
    }
    // SAFETY: `gui` is a live `GUITHREADINFO` whose `cbSize` is set; thread 0
    // means the calling thread, which is the one sending the input.
    unsafe { window::GetGUIThreadInfo(0, &mut gui) }.map_err(|_| UnsafeToSend::DesktopChanged)?;
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
    if let Some(Point { x, y }) = target {
        let point = windows::Win32::Foundation::POINT { x, y };
        // SAFETY: maps a point to the window under it; takes the point by value.
        let window_under = unsafe { window::WindowFromPoint(point) };
        check_window(window_under, avatar_pid)?;
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
    // SAFETY: `target` was validated above; the call returns the owning
    // top-level window and takes no pointer.
    let owner = unsafe { window::GetAncestor(target, window::GA_ROOTOWNER) };
    for hwnd in [target, owner] {
        let mut pid = 0;
        // SAFETY: `hwnd` came from a live query and `pid` is a stack local
        // the call only writes into.
        let thread = unsafe { window::GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if hwnd.is_invalid() || thread == 0 {
            return Err(UnsafeToSend::NoTarget);
        }
        if pid == std::process::id() || Some(pid) == avatar_pid {
            return Err(UnsafeToSend::EneWindow);
        }
    }
    Ok(())
}

/// Whether an event is a release of the press before it.
fn is_release(event: &input::INPUT) -> bool {
    match event.r#type {
        // SAFETY: the event's type tag selects the `ki` member, which is the
        // invariant `INPUT` requires of any correctly constructed value.
        input::INPUT_KEYBOARD => unsafe {
            event.Anonymous.ki.dwFlags.contains(input::KEYEVENTF_KEYUP)
        },
        // SAFETY: as above, for the `mi` member.
        input::INPUT_MOUSE => unsafe {
            matches!(
                event.Anonymous.mi.dwFlags,
                input::MOUSEEVENTF_LEFTUP | input::MOUSEEVENTF_RIGHTUP
            )
        },
        _ => false,
    }
}
