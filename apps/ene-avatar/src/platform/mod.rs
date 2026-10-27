//! OS-specific desktop integration.

#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
pub(crate) mod wayland;

#[cfg(any(windows, test))]
mod windows_outputs;

#[cfg(any(windows, test))]
mod windows_input;
