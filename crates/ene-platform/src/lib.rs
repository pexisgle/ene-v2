//! Protected filesystem access shared by the host and plugin loader.
#[cfg_attr(unix, path = "storage_unix.rs")]
#[cfg_attr(windows, path = "storage_windows.rs")]
pub mod storage;
