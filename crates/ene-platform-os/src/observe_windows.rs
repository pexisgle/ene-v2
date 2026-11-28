//! What the OS reports about the foreground window and user activity.
//!
//! Exposes plain data only; no handle or pointer crosses this boundary.

use crate::{Observation, ObservedWindow, Window};
use windows::Win32::{
    Foundation::{HWND, MAX_PATH},
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
    if crate::windows::unlocked().is_err() {
        return Observation::default();
    }
    Observation {
        window: foreground(),
        idle_seconds: idle_seconds(),
    }
}

/// The foreground window's title and process name. Either may be empty:
/// a window the OS will not describe is reported as it is, not guessed.
fn foreground() -> ObservedWindow {
    // SAFETY: reads the foreground window handle; no pointer is involved and
    // an absent window is reported as such.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd == HWND::default() {
        return ObservedWindow::Empty;
    }
    let mut buffer = [0u16; MAX_PATH as usize];
    // SAFETY: `buffer` is a live, correctly sized UTF-16 buffer that the call
    // fills and returns the written length for.
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    let title = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
    let mut pid = 0u32;
    // SAFETY: writes the owning process id into a live local; the window came
    // from the query above.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let app_id = process_name(pid).unwrap_or_default();
    if title.is_empty() && app_id.is_empty() {
        return ObservedWindow::Unavailable;
    }
    ObservedWindow::Window(Window { title, app_id })
}

/// The executable's file name, which is the app id a rule matches on.
pub(crate) fn process_name(pid: u32) -> Option<String> {
    // SAFETY: opens a query-only handle to the given pid; no memory is
    // borrowed and a failure is returned rather than used.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    // SAFETY: the successful OpenProcess call transferred ownership.
    let handle = unsafe { windows::core::Owned::new(handle) };
    let mut buffer = [0u16; MAX_PATH as usize];
    let mut size = buffer.len() as u32;
    // SAFETY: `buffer` is live and its capacity is passed in `size`, which the
    // call updates to what it wrote.
    let result = unsafe {
        QueryFullProcessImageNameW(
            *handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    result.ok()?;
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    path.rsplit(['\\', '/']).next().map(ToOwned::to_owned)
}

pub(crate) fn last_input_tick() -> Option<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a live `LASTINPUTINFO` whose `cbSize` is set, which is
    // the contract the call documents.
    unsafe { GetLastInputInfo(&mut info) }
        .as_bool()
        .then_some(info.dwTime)
}

fn idle_seconds() -> Option<u64> {
    let last_input = last_input_tick()?;
    // SAFETY: reads the system tick counter; takes no pointer.
    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
    Some(u64::from(now.wrapping_sub(last_input)) / 1000)
}
