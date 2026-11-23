//! What the OS reports about the foreground window and user activity.
//!
//! Exposes plain data only; no handle or pointer crosses this boundary.

#![cfg(windows)]

use crate::observe_types::{Observation, Window};
use windows::Win32::{
    Foundation::{CloseHandle, HWND, MAX_PATH},
    System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::{
        Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
        WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId},
    },
};

pub fn observe() -> Observation {
    Observation {
        window: foreground(),
        idle_seconds: idle_seconds(),
    }
}

/// The foreground window's title and process name. Either may be empty:
/// a window the OS will not describe is reported as it is, not guessed.
fn foreground() -> Option<Window> {
    // SAFETY: the calls read OS state into locals; every handle is closed.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() {
            return None;
        }
        let mut buffer = [0u16; MAX_PATH as usize];
        let length = GetWindowTextW(hwnd, &mut buffer);
        let title = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let app_id = process_name(pid).unwrap_or_default();
        if title.is_empty() && app_id.is_empty() {
            return None;
        }
        Some(Window { title, app_id })
    }
}

/// The executable's file name, which is the app id a rule matches on.
fn process_name(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed on every path below.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; MAX_PATH as usize];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ignored = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        path.rsplit(['\\', '/']).next().map(ToOwned::to_owned)
    }
}

fn idle_seconds() -> u64 {
    // SAFETY: `LASTINPUTINFO` is a plain struct filled by the OS.
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut info).as_bool() {
            return 0;
        }
        let now = windows::Win32::System::SystemInformation::GetTickCount();
        u64::from(now.wrapping_sub(info.dwTime)) / 1000
    }
}
