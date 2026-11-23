//! Overlay window adjustments for the frameless companion pod.
//!
//! Callers get an opaque [`PodWindow`]; no `HWND` or pointer crosses this
//! boundary, so the app crate can forbid `unsafe` entirely.

#![cfg(windows)]

use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        EnumThreadWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos,
        WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    },
};

/// The title the pod window is created with.
const POD_TITLE: &str = "ene-companion-pod";

/// A found pod window. Holding one borrows nothing; the OS owns the window.
#[derive(Clone, Copy, Debug)]
pub struct PodWindow(HWND);

impl PodWindow {
    /// Finds the pod window on the calling thread, if it exists yet.
    pub fn current() -> Option<Self> {
        unsafe extern "system" fn find_pod(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
            let mut buf = [0u16; 64];
            // SAFETY: `buf` is a valid, correctly sized UTF-16 buffer and
            // `GetWindowTextW` writes at most `buf.len()` code units into it.
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
            if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == POD_TITLE {
                let slot = lparam.0 as *mut Option<HWND>;
                // SAFETY: `lparam` carries a `*mut Option<HWND>` that outlives
                // the enumeration; `current` holds it on its own stack frame.
                unsafe { *slot = Some(hwnd) };
                return false.into();
            }
            true.into()
        }
        let mut found: Option<HWND> = None;
        let lparam = LPARAM(&raw mut found as isize);
        // SAFETY: `GetCurrentThreadId` takes no pointer and cannot fail.
        let thread_id = unsafe { GetCurrentThreadId() };
        // SAFETY: `find_pod` matches `WNDENUMPROC` and only writes through the
        // `lparam` pointer described above.
        unsafe {
            let _ = EnumThreadWindows(thread_id, Some(find_pod), lparam);
        }
        found.map(Self)
    }

    /// The window's bottom edge, in virtual-screen pixels.
    pub fn bottom(&self) -> Option<i32> {
        let mut rect = RECT::default();
        // SAFETY: `rect` is a valid, properly aligned `RECT` that
        // `GetWindowRect` only writes to.
        unsafe { GetWindowRect(self.0, &mut rect) }.ok()?;
        Some(rect.bottom)
    }

    /// Moves the window so its bottom edge sits at `target_bottom`.
    pub fn anchor_bottom(self, target_bottom: i32) {
        let Some(current) = self.bottom() else {
            return;
        };
        let mut rect = RECT::default();
        // SAFETY: as in `bottom`.
        if unsafe { GetWindowRect(self.0, &mut rect) }.is_err() {
            return;
        }
        let height = current.saturating_sub(rect.top);
        let new_top = (target_bottom - height).max(16);
        // SAFETY: `self.0` came from a live enumeration, so it stays a valid
        // window handle for as long as the pod exists.
        unsafe {
            let _ = SetWindowPos(
                self.0,
                Some(HWND_TOPMOST),
                rect.left,
                new_top,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// Shows or hides the pod without taking focus.
    pub fn set_visible(self, visible: bool) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};
        // SAFETY: `self.0` is a live window handle; `ShowWindow` only changes
        // that window's visibility.
        unsafe {
            let _ = ShowWindow(self.0, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        if visible {
            self.keep_topmost();
        }
    }

    /// Keeps the pod above other windows without activating it.
    pub fn keep_topmost(self) {
        // SAFETY: `self.0` is a live window handle (see `current`).
        unsafe {
            let _ = SetWindowPos(
                self.0,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// Drops the DWM frame, rounding and border so the pod looks frameless.
    pub fn suppress_dwm_frame(self) {
        // SAFETY: `self.0` is a live window handle; the style bits are read from
        // and written back to that same window.
        // SAFETY: reads the extended style of a live window; the value is a
        // plain integer and nothing is dereferenced.
        let style = unsafe { GetWindowLongPtrW(self.0, GWL_EXSTYLE) };
        // SAFETY: writes the adjusted bits back to the same live window.
        unsafe {
            SetWindowLongPtrW(
                self.0,
                GWL_EXSTYLE,
                (style & !(WS_EX_TOOLWINDOW.0 as isize)) | WS_EX_APPWINDOW.0 as isize,
            );
        }
        let ncrp: u32 = 1; // DWMNCRP_DISABLED
        let corner: u32 = 1; // DWMWCP_DONOTROUND
        let color: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        // SAFETY: `ncrp` is a live local `u32` and the size passed matches it.
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.0.0,
                DWMWA_NCRENDERING_POLICY,
                (&raw const ncrp).cast(),
                4,
            );
        }
        // SAFETY: `corner` is a live local `u32` and the size passed matches it.
        unsafe {
            let _ = DwmSetWindowAttribute(
                self.0.0,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&raw const corner).cast(),
                4,
            );
        }
        // SAFETY: `color` is a live local `u32` and the size passed matches it.
        unsafe {
            let _ =
                DwmSetWindowAttribute(self.0.0, DWMWA_BORDER_COLOR, (&raw const color).cast(), 4);
        }
    }
}

const DWMWA_NCRENDERING_POLICY: u32 = 2;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWA_BORDER_COLOR: u32 = 34;

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut std::ffi::c_void,
        dw_attribute: u32,
        pv_attribute: *const std::ffi::c_void,
        cb_attribute: u32,
    ) -> i32;
}
