//! File reads and metadata stamps for verified packages.
use super::paths::relative;
use crate::{Error, Result};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{io::Read, path::Path, time::SystemTime};

#[derive(Clone, Debug)]
pub(super) struct Stamp {
    pub(super) len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}
#[cfg(unix)]
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        dev: metadata.dev(),
        ino: metadata.ino(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}
#[cfg(not(unix))]
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}
#[cfg(unix)]
pub(super) fn unchanged(metadata: &std::fs::Metadata, stamp: &Stamp) -> bool {
    metadata.is_file()
        && metadata.len() == stamp.len
        && metadata.modified().ok() == stamp.modified
        && metadata.dev() == stamp.dev
        && metadata.ino() == stamp.ino
        && metadata.ctime() == stamp.ctime
        && metadata.ctime_nsec() == stamp.ctime_nsec
}
#[cfg(not(unix))]
fn unchanged(metadata: &std::fs::Metadata, stamp: &Stamp) -> bool {
    metadata.is_file() && metadata.len() == stamp.len && metadata.modified().ok() == stamp.modified
}
pub(super) fn owned(path: &Path, directory: bool) -> Result<()> {
    ene_platform::storage::check(path, directory, false).map_err(|_| Error::Invalid)
}
pub(super) fn read(root: &Path, path: &str, limit: u64) -> Result<(Vec<u8>, Stamp)> {
    if !relative(path) {
        return Err(Error::Invalid);
    }
    let file = open_code(root, path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.len() > limit {
        return Err(Error::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid);
    }
    Ok((bytes, stamp(&m)))
}
pub(super) fn open_code(root: &Path, path: &str) -> Result<std::fs::File> {
    let mut parent = root.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for part in &components[..components.len() - 1] {
        parent.push(part);
        owned(&parent, true)?;
    }
    Ok(ene_platform::storage::open(&root.join(path), false, false)?)
}
