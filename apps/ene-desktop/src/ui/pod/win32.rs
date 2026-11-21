use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        EnumThreadWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos,
        WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    },
};

pub(super) fn apply_hwnd_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub(super) fn current_pod_hwnd() -> Option<HWND> {
    unsafe extern "system" fn find_pod(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let mut buf = [0u16; 64];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "ene-companion-pod" {
            let slot = lparam.0 as *mut Option<HWND>;
            unsafe { *slot = Some(hwnd) };
            return false.into();
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    let lparam = LPARAM(&raw mut found as isize);
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(find_pod), lparam);
    }
    found
}

pub(super) fn read_window_bottom(hwnd: HWND) -> Option<i32> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect.bottom)
}

pub(super) fn anchor_window_bottom(hwnd_raw: isize, target_bottom: i32) {
    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let height = rect.bottom - rect.top;
    let new_top = (target_bottom - height).max(16);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.left,
            new_top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub(super) fn suppress_dwm_frame(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (style & !(WS_EX_TOOLWINDOW.0 as isize)) | WS_EX_APPWINDOW.0 as isize,
        );
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: *mut std::ffi::c_void,
            dw_attribute: u32,
            pv_attribute: *const std::ffi::c_void,
            cb_attribute: u32,
        ) -> i32;
    }
    const DWMWA_NCRENDERING_POLICY: u32 = 2;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    let ncrp: u32 = 1; // DWMNCRP_DISABLED
    let corner: u32 = 1; // DWMWCP_DONOTROUND
    let color: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_NCRENDERING_POLICY,
            (&raw const ncrp).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(hwnd.0, DWMWA_BORDER_COLOR, (&raw const color).cast(), 4);
    }
}
