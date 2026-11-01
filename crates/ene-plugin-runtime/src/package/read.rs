//! File reads, metadata stamps and hashing for verified packages.
use super::paths::relative;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
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
pub(super) fn hash_file(mut file: std::fs::File, limit: u64) -> Result<(String, Stamp)> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::Invalid);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(Error::Invalid);
        }
        digest.update(&buffer[..read]);
    }
    Ok((format!("{:x}", digest.finalize()), stamp(&metadata)))
}
