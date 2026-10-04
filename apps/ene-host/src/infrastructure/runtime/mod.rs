pub mod paths;
use atomic_write_file::{AtomicWriteFile, unix::OpenOptionsExt as _};
use ene_api_types::host::RuntimeConnection;
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{
    fs::{self, File, Metadata},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub struct Runtime {
    directory: File,
    _lock: File,
    pub token: String,
}
fn check_metadata(metadata: Metadata, is_dir: bool) -> io::Result<()> {
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || if is_dir {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ene file must be owned by the current user, private, regular, and singly linked",
        ));
    }
    Ok(())
}
fn directory(path: &Path) -> io::Result<File> {
    let file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    check_metadata(file.metadata()?, true)?;
    Ok(file)
}
pub fn private_directory(path: &Path) -> io::Result<File> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    directory(path)
}
fn private_at(directory: &File, name: &Path) -> io::Result<File> {
    let file = File::from(rustix::fs::openat(
        directory,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )?);
    check_metadata(file.metadata()?, false)?;
    Ok(file)
}
pub fn private_file(path: &Path) -> io::Result<File> {
    let parent =
        directory(path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing parent directory")
        })?)?;
    private_at(
        &parent,
        Path::new(
            path.file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?,
        ),
    )
}
impl Runtime {
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let root = directory(root)?;
        match rustix::fs::mkdirat(&root, "ene-host", Mode::RWXU) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let directory = File::from(rustix::fs::openat(
            &root,
            "ene-host",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        check_metadata(directory.metadata()?, true)?;
        let lock = private_at(&directory, Path::new("host.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "ene-host is already running",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        // Check the opened stale descriptor before unlinking, under the singleton lock.
        match rustix::fs::openat(
            &directory,
            "connection.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => {
                check_metadata(File::from(fd).metadata()?, false)?;
                rustix::fs::unlinkat(&directory, "connection.json", AtFlags::empty())?;
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            directory,
            _lock: lock,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        })
    }
    fn descriptor(&self) -> PathBuf {
        // Linux procfs resolves this owned directory FD even after a path rename.
        PathBuf::from(format!(
            "/proc/self/fd/{}/connection.json",
            self.directory.as_raw_fd()
        ))
    }
    pub fn publish(&self, endpoint: String) -> io::Result<()> {
        let mut file = AtomicWriteFile::options()
            .mode(0o600)
            .preserve_mode(false)
            .preserve_owner(false)
            .open(self.descriptor())?;
        serde_json::to_writer(
            &mut file,
            &RuntimeConnection {
                endpoint,
                token: self.token.clone(),
            },
        )?;
        file.write_all(b"\n")?;
        file.commit()
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(&self.directory, "connection.json", AtFlags::empty());
    }
}

#[cfg(test)]
mod fd_tests {
    use super::*;
    fn private_temp() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }
    #[test]
    fn descriptor_publish_and_cleanup_stay_in_acquired_directory_after_path_replacement() {
        let temp = private_temp();
        let runtime = Runtime::acquire(temp.path()).unwrap();
        fs::rename(temp.path().join("ene-host"), temp.path().join("moved")).unwrap();
        private_directory(&temp.path().join("ene-host")).unwrap();
        let replacement = temp.path().join("ene-host/connection.json");
        fs::write(&replacement, b"unrelated descriptor").unwrap();
        runtime.publish("http://127.0.0.1:1234/v1".into()).unwrap();
        let descriptor = temp.path().join("moved/connection.json");
        assert!(descriptor.exists());
        assert_eq!(fs::read(&replacement).unwrap(), b"unrelated descriptor");
        drop(runtime);
        assert!(!descriptor.exists());
        assert_eq!(fs::read(replacement).unwrap(), b"unrelated descriptor");
    }
}
