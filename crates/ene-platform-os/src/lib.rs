//! OS integration behind safe abstractions.
//!
//! The host and desktop crates forbid `unsafe`; every platform call they need
//! lives here instead, wrapped so the caller never sees a raw handle, pointer
//! or `unsafe` block.
//!
//! Each module states the invariant its `unsafe` relies on in a `SAFETY:`
//! comment, and exposes only safe methods over owned values.

pub mod tray;
#[cfg(windows)]
pub mod window;

pub use tray::{Tray, TrayAction, TrayHandle, TrayMode, TraySection, TrayState};
#[cfg(windows)]
pub use window::PodWindow;
