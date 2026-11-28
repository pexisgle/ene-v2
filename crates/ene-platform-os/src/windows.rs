use ::windows::Win32::{
    Foundation::{HANDLE, HWND, POINT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow},
    System::StationsAndDesktops::{
        DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
        UOI_NAME,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{
            GA_ROOT, GUITHREADINFO, GWL_EXSTYLE, GetAncestor, GetCursorPos, GetForegroundWindow,
            GetGUIThreadInfo, GetWindowLongPtrW, GetWindowThreadProcessId, SetWindowLongPtrW,
            WS_EX_NOACTIVATE, WindowFromPoint,
        },
    },
};
use anyhow::{Context, Result, ensure};
use std::num::NonZeroIsize;

pub struct ForegroundMonitor {
    pub focus: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub input_tick: u32,
}

struct Dpi(DPI_AWARENESS_CONTEXT);

impl Drop for Dpi {
    fn drop(&mut self) {
        // SAFETY: restores the value returned for this thread.
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

pub fn physical<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    // SAFETY: changes only the calling thread's DPI context.
    let prior = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    ensure!(
        !prior.is_invalid(),
        "cannot enter physical desktop coordinates"
    );
    let _restore = Dpi(prior);
    f()
}

pub(crate) fn unlocked() -> Result<()> {
    // SAFETY: the read-only desktop handle is owned after successful return.
    let handle = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)? };
    // SAFETY: this handle was newly allocated by OpenInputDesktop.
    let handle = unsafe { ::windows::core::Owned::new(handle) };
    let mut name = [0u16; 64];
    // SAFETY: the size is the capacity in bytes of the writable UTF-16 buffer.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(handle.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of_val(&name) as u32,
            None,
        )?;
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    ensure!(
        String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("default"),
        "screen is locked"
    );
    Ok(())
}

pub fn foreground_monitor() -> Result<ForegroundMonitor> {
    unlocked()?;
    // SAFETY: all pointers are HWND values issued by Windows or live output buffers.
    let (window, info, pid) = unsafe {
        let window = GetForegroundWindow();
        ensure!(!window.is_invalid(), "no focused application");
        let display = MonitorFromWindow(window, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        ensure!(
            !display.is_invalid() && GetMonitorInfoW(display, &mut info).as_bool(),
            "foreground monitor unavailable"
        );
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        (window, info, pid)
    };
    let process = crate::sensor::process_name(pid).context("focused application is unavailable")?;
    ensure!(
        pid != std::process::id() && !process.eq_ignore_ascii_case("ene-avatar.exe"),
        "cannot automate Ene itself"
    );
    Ok(ForegroundMonitor {
        focus: window.0 as usize as u64,
        x: info.rcMonitor.left,
        y: info.rcMonitor.top,
        width: info.rcMonitor.right.abs_diff(info.rcMonitor.left),
        height: info.rcMonitor.bottom.abs_diff(info.rcMonitor.top),
        input_tick: crate::sensor::last_input_tick().context("last user input unavailable")?,
    })
}

pub fn check_target(point: Option<(i32, i32)>, focus: u64) -> Result<()> {
    if let Some((x, y)) = point {
        // SAFETY: WindowFromPoint issues the HWND read by GetAncestor.
        let window = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
        ensure!(
            window == HWND(focus as usize as *mut _),
            "pointer target is outside the captured application"
        );
    }
    Ok(())
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: the call writes into a live POINT.
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

pub fn set_non_activating(handle: NonZeroIsize) {
    let hwnd = HWND(handle.get() as *mut _);
    let mut pid = 0;
    // SAFETY: Windows validates HWND values. The process ID is a live output buffer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != std::process::id() {
        return;
    }
    // SAFETY: HWND denotes a window in this process. Both functions use only its extended style.
    unsafe {
        let flags = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let needed = flags | WS_EX_NOACTIVATE.0;
        if needed != flags {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, needed as isize);
        }
    }
}

pub fn has_pointer_capture(handle: NonZeroIsize) -> bool {
    let hwnd = HWND(handle.get() as *mut _);
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: Windows validates HWND values. GUITHREADINFO is live and initialized with its size.
    unsafe {
        let thread = GetWindowThreadProcessId(hwnd, None);
        thread != 0 && GetGUIThreadInfo(thread, &mut info).is_ok() && info.hwndCapture == hwnd
    }
}
