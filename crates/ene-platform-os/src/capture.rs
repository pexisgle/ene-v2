//! Screen capture of the foreground window.
//!
//! Every Win32 handle is created and released inside this module; callers
//! see owned bytes, plain integers, and an opaque foreground handle.

#![cfg(windows)]

/// Maximum width or height of a reported JPEG frame.
const MAX_FRAME_EDGE: u32 = 1280;
/// Maximum encoded JPEG size before it is rejected.
const MAX_JPEG_BYTES: usize = 512 * 1024;

use crate::Rect;
use image::{DynamicImage, ExtendedColorType, codecs::jpeg::JpegEncoder};
use std::mem::size_of;
use windows::Win32::{
    Foundation::{HANDLE, HWND, RECT},
    Graphics::Gdi::{self, MONITOR_DEFAULTTONULL, MONITORINFO},
    System::StationsAndDesktops::{
        DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
        UOI_NAME,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::GetForegroundWindow,
    },
};

pub fn unlocked() -> bool {
    let mut name = [0u16; 64];
    // SAFETY: opens the calling thread's input desktop for reading only; no
    // memory is borrowed and a sealed desktop is reported as a failure.
    let Ok(desktop) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) })
    else {
        return false;
    };
    // SAFETY: `OpenInputDesktop` returned a newly owned handle.
    let desktop = unsafe { windows::core::Owned::new(desktop) };
    // SAFETY: `name` is a live 64-element UTF-16 buffer whose capacity is
    // passed explicitly, so the call cannot write past it.
    let result = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of::<[u16; 64]>() as u32,
            None,
        )
    };
    result.is_ok()
        && String::from_utf16_lossy(&name[..name.iter().position(|&ch| ch == 0).unwrap_or(64)])
            .eq_ignore_ascii_case("default")
}

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub monitor: Rect,
    pub window: isize,
}

struct RestoreDpi(DPI_AWARENESS_CONTEXT);

impl Drop for RestoreDpi {
    fn drop(&mut self) {
        // SAFETY: this is the previous DPI context of the current thread.
        let _previous = unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

pub fn physical<T>(action: impl FnOnce() -> T) -> Option<T> {
    // SAFETY: sets only the calling thread's DPI context.
    let context =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if context.is_invalid() {
        return None;
    }
    let _restore = RestoreDpi(context);
    Some(action())
}

pub(crate) fn foreground() -> Option<(HWND, RECT)> {
    // SAFETY: reads the foreground window handle; no pointer is involved.
    let window = unsafe { GetForegroundWindow() };
    // SAFETY: maps a live window handle to its monitor; takes no pointer.
    let monitor = unsafe { Gdi::MonitorFromWindow(window, MONITOR_DEFAULTTONULL) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live `MONITORINFO` whose `cbSize` is set, and the
    // monitor handle was validated just above.
    let ok = unsafe { Gdi::GetMonitorInfoW(monitor, &mut info).as_bool() };
    if !window.is_invalid() && !monitor.is_invalid() && ok {
        Some((window, info.rcMonitor))
    } else {
        None
    }
}

pub fn capture_frame() -> Option<CapturedFrame> {
    if !unlocked() {
        return None;
    }
    let (pixels, window, monitor) = physical(|| {
        let (window, rect) = foreground()?;
        let bounds = Rect::from(rect);
        let monitor = xcap::Monitor::from_point(bounds.left, bounds.top).ok()?;
        let pixels = monitor.capture_image().ok()?;
        if pixels.dimensions()
            != (
                bounds.left.abs_diff(bounds.right),
                bounds.top.abs_diff(bounds.bottom),
            )
        {
            return None;
        }
        Some((pixels, window.0 as isize, bounds))
    })??;
    let pixels = DynamicImage::ImageRgba8(pixels).into_rgb8();
    let (width, height) = pixels.dimensions();
    let edge = width.max(height);
    let pixels = if edge > MAX_FRAME_EDGE {
        let scale = f64::from(MAX_FRAME_EDGE) / f64::from(edge);
        let scaled = |size| (f64::from(size) * scale).round().max(1.0) as u32;
        image::imageops::resize(
            &pixels,
            scaled(width),
            scaled(height),
            image::imageops::FilterType::Triangle,
        )
    } else {
        pixels
    };
    let (width, height) = pixels.dimensions();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 75)
        .encode(pixels.as_raw(), width, height, ExtendedColorType::Rgb8)
        .ok()?;
    (unlocked() && jpeg.len() <= MAX_JPEG_BYTES).then_some(CapturedFrame {
        jpeg,
        width,
        height,
        monitor,
        window,
    })
}
