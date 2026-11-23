//! Screen capture of the foreground window.
//!
//! The Win32 and GDI work lives in `ene_platform_os::capture`; this module
//! only re-exports it under the names perception code already uses.

#[cfg(windows)]
pub(crate) use ene_platform_os::capture::{capture_frame, physical, unlocked};

/// One encoded JPEG of the foreground window, if the desktop is usable.
pub(crate) fn capture() -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        capture_frame().map(|frame| frame.jpeg)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(not(windows))]
pub(crate) fn unlocked() -> bool {
    true
}

#[cfg(not(windows))]
pub(crate) fn physical<T>(action: impl FnOnce() -> T) -> Option<T> {
    Some(action())
}
