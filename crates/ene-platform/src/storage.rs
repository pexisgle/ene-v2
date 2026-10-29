//! Handle-based validation. Private files and package code have different ACL policies.
use std::{fs::File, io, path::Path};
#[cfg(windows)]
#[path = "storage_windows.rs"]
pub(crate) mod os;
#[cfg(unix)]
#[path = "storage_unix.rs"]
mod os;
#[cfg(windows)]
pub use os::open_snapshot;
pub use os::{check, create_private_dir_all, freeze, open, private_file, write_atomic};

pub fn read(path: &Path, limit: u64, private: bool) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = open(path, false, private)?;
    if file.metadata()?.len() > limit {
        return Err(io::Error::other("file exceeds size limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("file exceeds size limit"));
    }
    Ok(bytes)
}
pub fn private_directory(path: &Path) -> io::Result<File> {
    create_private_dir_all(path)?;
    open(path, true, true)
}
