//! Native services and operating-system adapters.
pub mod avatar;
pub mod file_picker;
pub mod host;
pub mod platform;
pub mod settings;
#[cfg_attr(windows, path = "tray_windows.rs")]
pub mod tray;
