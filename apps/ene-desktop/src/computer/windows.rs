use super::*;
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
            GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, WindowFromPoint,
        },
    },
};

struct Dpi(DPI_AWARENESS_CONTEXT);

impl Drop for Dpi {
    fn drop(&mut self) {
        // SAFETY: restores the value returned for this thread.
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

pub(super) fn physical<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    // SAFETY: changes only the calling thread's DPI context.
    let prior = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    ensure!(
        !prior.is_invalid(),
        "cannot enter physical desktop coordinates"
    );
    let _restore = Dpi(prior);
    f()
}

pub(super) fn unlocked() -> Result<()> {
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

pub(super) fn desktop() -> Result<Desktop> {
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
    let process =
        crate::observe::process_name(pid).context("focused application is unavailable")?;
    ensure!(
        pid != std::process::id() && !process.eq_ignore_ascii_case("ene-avatar.exe"),
        "cannot automate Ene itself"
    );
    Ok(Desktop {
        focus: window.0 as usize as u64,
        region: Bounds {
            x: info.rcMonitor.left,
            y: info.rcMonitor.top,
            width: info.rcMonitor.right.abs_diff(info.rcMonitor.left),
            height: info.rcMonitor.bottom.abs_diff(info.rcMonitor.top),
        },
        activity: None,
        tick: Some(crate::observe::last_input_tick().context("last user input unavailable")?),
    })
}

pub(super) fn check_target(point: Option<(i32, i32)>, focus: u64) -> Result<()> {
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

pub(super) fn capture(region: Bounds) -> Result<RgbImage> {
    let monitor = xcap::Monitor::from_point(region.x, region.y)?;
    let pixels = monitor.capture_image()?;
    ensure!(
        pixels.dimensions() == (region.width, region.height),
        "foreground monitor changed"
    );
    Ok(image::DynamicImage::ImageRgba8(pixels).into_rgb8())
}
