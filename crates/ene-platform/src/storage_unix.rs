use rustix::fs::{Mode, OFlags};
use std::{
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
};
pub fn check(path: &Path, directory: bool, private: bool) -> io::Result<()> {
    open(path, directory, private).map(drop)
}
pub fn open(path: &Path, directory: bool, private: bool) -> io::Result<File> {
    let flags = OFlags::RDONLY
        | OFlags::NOFOLLOW
        | OFlags::CLOEXEC
        | OFlags::NONBLOCK
        | if directory {
            OFlags::DIRECTORY
        } else {
            OFlags::empty()
        };
    let file = File::from(rustix::fs::open(path, flags, Mode::empty())?);
    let m = file.metadata()?;
    if m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & if private { 0o077 } else { 0o022 } != 0
        || directory != m.is_dir()
        || !directory && (!m.is_file() || m.nlink() != 1)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe ene file ownership, mode or type",
        ));
    }
    Ok(file)
}
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    check(path, true, true)
}
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    check(
        path.parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
        true,
        true,
    )?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    check(path, false, true)?;
    Ok(file)
}
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    create_private_dir_all(parent)?;
    if path.exists() {
        check(path, false, true)?;
    }
    let tmp = parent.join(format!(".ene-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()
    })();
    let _ = fs::remove_file(tmp);
    result
}
