//! GPU frame capture, exact alpha masks, and Wayland presentation.
mod frames;
mod presenter;
mod surface;

pub use frames::GpuFrames;
pub use presenter::AvatarPresenter;
use std::fmt;

#[derive(Debug, Clone)]
pub struct GpuError(pub String);
impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for GpuError {}

#[cfg(all(test, windows))]
mod windows_tests;
