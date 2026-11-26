//! OS integration behind safe abstractions.
//!
//! The host and desktop crates forbid `unsafe`; every platform call they need
//! lives here instead, wrapped so the caller never sees a raw handle, pointer
//! or `unsafe` block.
//!
//! Each module states the invariant its `unsafe` relies on in a `SAFETY:`
//! comment, and exposes only safe methods over owned values.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod observe;
#[cfg(target_os = "linux")]
mod observe_linux;
pub mod observe_types;
pub mod tray;
#[cfg(windows)]
pub mod window;

#[cfg(windows)]
pub use capture::{CapturedFrame, Foreground, MAX_FRAME_EDGE, MAX_JPEG_BYTES};
#[cfg(windows)]
pub use input::{Desktop, Point, Rect};
#[cfg(windows)]
pub use observe::observe;
#[cfg(target_os = "linux")]
pub use observe_linux::{observe, supported as observation_supported};
pub use observe_types::{Observation, Window};
pub use tray::{Tray, TrayAction, TrayHandle, TrayMode, TraySection, TrayState};
#[cfg(windows)]
pub use window::PodWindow;
