#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(windows, path = "windows.rs")]
mod os;
pub use os::{Runtime, private_directory, private_file};
