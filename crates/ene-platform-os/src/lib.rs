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
#![warn(clippy::needless_pass_by_value)]
pub mod best_effort;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod observe;
pub mod observe_types;
pub mod tray;
#[cfg(windows)]
pub mod window;

pub use best_effort::{debug_if_err, ignore_teardown, warn_if_err};
#[cfg(windows)]
pub use capture::{CapturedFrame, Foreground, MAX_FRAME_EDGE, MAX_JPEG_BYTES};
#[cfg(windows)]
pub use input::{Desktop, Rect, UnsafeToSend, check_before_send, send_input};
#[cfg(windows)]
pub use observe::observe;
pub use observe_types::{Observation, Window};
pub use tray::{Tray, TrayAction, TrayHandle, TrayMode, TraySection, TrayState};
#[cfg(windows)]
pub use window::PodWindow;
