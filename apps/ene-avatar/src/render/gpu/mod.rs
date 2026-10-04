//! GPU frame capture, exact alpha masks, and Wayland presentation.
mod frames;
mod surface;

pub use frames::GpuFrames;
use std::fmt;
pub use surface::GpuSurface;

#[derive(Debug, Clone)]
pub struct GpuError(pub String);
impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for GpuError {}
