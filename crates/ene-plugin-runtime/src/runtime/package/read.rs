//! File reads and metadata stamps for verified packages.
use super::paths::relative;
use crate::runtime::Result;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, File as CapFile, Metadata, MetadataExt as _, OpenOptions},
    time::SystemTime,
};
use std::{io::Read, path::Path};

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
fn stamp(metadata: &Metadata) -> Stamp {
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
fn stamp(metadata: &Metadata) -> Stamp {
    Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}
#[cfg(unix)]
pub(super) fn unchanged(metadata: &Metadata, stamp: &Stamp) -> bool {
    metadata.is_file()
        && metadata.len() == stamp.len
        && metadata.modified().ok() == stamp.modified
        && metadata.dev() == stamp.dev
        && metadata.ino() == stamp.ino
        && metadata.ctime() == stamp.ctime
        && metadata.ctime_nsec() == stamp.ctime_nsec
}
#[cfg(not(unix))]
pub(super) fn unchanged(metadata: &Metadata, stamp: &Stamp) -> bool {
    metadata.is_file() && metadata.len() == stamp.len && metadata.modified().ok() == stamp.modified
}

#[cfg(windows)]
fn reparse_free(metadata: &Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

fn owned_dir(dir: &Dir) -> Result<()> {
    let metadata = dir.dir_metadata()?;
    if !metadata.is_dir() {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    #[cfg(unix)]
    {
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(anyhow::anyhow!("invalid_package"));
        }
    }
    #[cfg(windows)]
    if !reparse_free(&metadata) {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    Ok(())
}

fn owned_file(file: &CapFile) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || cap_fs_ext::MetadataExt::nlink(&metadata) != 1 {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    #[cfg(unix)]
    {
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(anyhow::anyhow!("invalid_package"));
        }
    }
    #[cfg(windows)]
    if !reparse_free(&metadata) {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    Ok(())
}

/// Open a package root and enforce the package ownership policy.
pub(super) fn open_root(root: &Path) -> Result<Dir> {
    let dir = match root.file_name() {
        Some(name) => {
            let parent = root
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            Dir::open_ambient_dir(parent, ambient_authority())?
                .open_dir_nofollow(name)
                .map_err(|_| anyhow::anyhow!("invalid_package"))?
        }
        None => Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|_| anyhow::anyhow!("invalid_package"))?,
    };
    owned_dir(&dir)?;
    Ok(dir)
}

pub(super) fn read(root: &Dir, path: &str, limit: u64) -> Result<(Vec<u8>, Stamp)> {
    if !relative(path) {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    let file = open_code(root, path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.len() > limit {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(anyhow::anyhow!("invalid_package"));
    }
    Ok((bytes, stamp(&m)))
}

pub(super) fn open_code(root: &Dir, path: &str) -> Result<CapFile> {
    let mut directory = root.try_clone()?;
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let file = directory
                .open_with(component.as_os_str(), &options)
                .map_err(|_| anyhow::anyhow!("invalid_package"))?;
            owned_file(&file)?;
            return Ok(file);
        }
        directory = directory
            .open_dir_nofollow(component.as_os_str())
            .map_err(|_| anyhow::anyhow!("invalid_package"))?;
        owned_dir(&directory)?;
    }
    Err(anyhow::anyhow!("invalid_package"))
}
