//! Screen capture of the foreground window.
//!
//! Every Win32 handle is created and released inside this module; callers
//! see owned bytes, plain integers, and an opaque foreground handle.

#![cfg(windows)]

/// Maximum width or height of a reported JPEG frame.
pub const MAX_FRAME_EDGE: u32 = 1280;
/// Maximum encoded JPEG size before it is rejected.
pub const MAX_JPEG_BYTES: usize = 512 * 1024;

use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use std::mem::size_of;
use windows::Win32::{
    Foundation::{HANDLE, HWND, RECT},
    Graphics::Gdi::{
        self, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HALFTONE, HDC,
        HGDIOBJ, MONITOR_DEFAULTTONULL, MONITORINFO, SRCCOPY,
    },
    System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
        OpenInputDesktop, UOI_NAME,
    },
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::GetForegroundWindow,
    },
};

pub fn unlocked() -> bool {
    let mut name = [0u16; 64];
    // SAFETY: the output is a sized UTF-16 buffer and the input desktop handle
    // is closed before returning. An inaccessible desktop is treated as locked.
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            return false;
        };
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of::<[u16; 64]>() as u32,
            None,
        );
        let _ = CloseDesktop(desktop);
        result.is_ok()
            && String::from_utf16_lossy(&name[..name.iter().position(|&ch| ch == 0).unwrap_or(64)])
                .eq_ignore_ascii_case("default")
    }
}

pub fn capture() -> Option<Vec<u8>> {
    capture_frame().map(|frame| frame.jpeg)
}

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub monitor: RECT,
    pub window: isize,
}

pub fn physical<T>(action: impl FnOnce() -> T) -> Option<T> {
    // SAFETY: both awareness contexts are used on this thread only.
    unsafe {
        let context = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if context.is_invalid() {
            return None;
        }
        let result = action();
        let _ = SetThreadDpiAwarenessContext(context);
        Some(result)
    }
}

fn foreground() -> Option<(HWND, RECT)> {
    // SAFETY: reads the foreground window and writes a sized monitor info struct.
    unsafe {
        let window = GetForegroundWindow();
        let monitor = Gdi::MonitorFromWindow(window, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        (!window.is_invalid()
            && !monitor.is_invalid()
            && Gdi::GetMonitorInfoW(monitor, &mut info).as_bool())
        .then_some((window, info.rcMonitor))
    }
}

/// The foreground window, with its handle kept opaque.
///
/// Only this crate's `input` module needs the raw handle back; it is wrapped
/// rather than exposed so the type cannot escape into the app.
#[derive(Clone, Copy, Debug)]
pub struct Foreground {
    /// The owning window, as an opaque integer.
    pub window: isize,
    rect: RECT,
}

impl Foreground {
    /// The monitor rectangle the window occupies.
    pub fn rect(self) -> RECT {
        self.rect
    }

    /// The raw handle, for this crate's own Win32 calls.
    pub(crate) fn window_handle(self) -> HWND {
        HWND(self.window as *mut std::ffi::c_void)
    }
}

/// The foreground window and the monitor rect it occupies.
pub fn foreground_rect() -> Option<Foreground> {
    foreground().map(|(window, rect)| Foreground {
        window: window.0 as isize,
        rect,
    })
}

pub fn capture_frame() -> Option<CapturedFrame> {
    if !unlocked() {
        return None;
    }
    let (pixels, width, height, window, monitor) = physical(|| {
        let (window, monitor) = foreground()?;
        // SAFETY: the rectangle was read from the monitor in this DPI context.
        let (pixels, width, height) = unsafe { pixels(monitor) }?;
        Some((pixels, width, height, window.0 as isize, monitor))
    })??;
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 75)
        .encode(&pixels, width, height, ExtendedColorType::Rgb8)
        .ok()?;
    (unlocked() && jpeg.len() <= MAX_JPEG_BYTES).then_some(CapturedFrame {
        jpeg,
        width,
        height,
        monitor,
        window,
    })
}

unsafe fn pixels(src: RECT) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: handles are checked before use. The memory DC owns no selected
    // bitmap on deletion.
    unsafe {
        let (src_width, src_height) = (src.right - src.left, src.bottom - src.top);
        if src_width <= 0 || src_height <= 0 {
            return None;
        }
        let limit = MAX_FRAME_EDGE as i32;
        let scale = f64::from(limit) / f64::from(src_width.max(src_height).max(limit));
        let width = ((f64::from(src_width) * scale).round() as i32).max(1);
        let height = ((f64::from(src_height) * scale).round() as i32).max(1);
        let desktop = Gdi::GetDC(None);
        if desktop.is_invalid() {
            return None;
        }
        let memory = Gdi::CreateCompatibleDC(Some(desktop));
        let result = if memory.is_invalid() {
            None
        } else {
            let result = copy(desktop, memory, src, width, height);
            let _ = Gdi::DeleteDC(memory);
            result
        };
        Gdi::ReleaseDC(None, desktop);
        result.map(|pixels| (pixels, width as u32, height as u32))
    }
}

unsafe fn copy(
    desktop: HDC,
    memory: HDC,
    src: windows::Win32::Foundation::RECT,
    width: i32,
    height: i32,
) -> Option<Vec<u8>> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: a top-down 32bpp DIB has four bytes per pixel and no row padding.
    // Flush GDI before reading the mapped pixels and restore the DC's selection
    // before freeing the bitmap.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap =
            Gdi::CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let object = HGDIOBJ(bitmap.0);
        let previous = Gdi::SelectObject(memory, object);
        let result = if previous.is_invalid() || bits.is_null() {
            None
        } else {
            let _ = Gdi::SetStretchBltMode(memory, HALFTONE);
            let copied = Gdi::StretchBlt(
                memory,
                0,
                0,
                width,
                height,
                Some(desktop),
                src.left,
                src.top,
                src.right - src.left,
                src.bottom - src.top,
                SRCCOPY | CAPTUREBLT,
            )
            .as_bool()
                && Gdi::GdiFlush().as_bool();
            copied.then(|| {
                let pixels =
                    std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize);
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
                    .collect()
            })
        };
        if !previous.is_invalid() {
            Gdi::SelectObject(memory, previous);
        }
        let _ = Gdi::DeleteObject(object);
        result
    }
}
