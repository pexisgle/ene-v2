use ene_protocol::host::RuntimeConnection;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub struct Runtime {
    directory: PathBuf,
    descriptor: PathBuf,
    _lock: File,
    pub token: String,
}

fn owner() -> u32 {
    // geteuid has no arguments, pointers, or side effects.
    unsafe { libc::geteuid() }
}

pub fn check_private(path: &Path, is_dir: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != owner()
        || metadata.permissions().mode() & 0o077 != 0
        || if is_dir {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ene path must be owned by the current user, private, and not a link",
        ));
    }
    Ok(())
}

use std::os::unix::fs::PermissionsExt;

pub fn private_directory(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_private(path, true)
}

pub fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    check_private(path, false)?;
    Ok(file)
}

impl Runtime {
    pub fn acquire(root: &Path) -> io::Result<Self> {
        check_private(root, true)?;
        let directory = root.join("ene-host");
        private_directory(&directory)?;
        let lock = private_file(&directory.join("host.lock"))?;
        lock.try_lock_exclusive().map_err(|_| {
            io::Error::new(io::ErrorKind::AlreadyExists, "ene-host is already running")
        })?;
        let descriptor = directory.join("connection.json");
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        // Remove a stale descriptor only after obtaining the singleton lock.
        if descriptor.exists() {
            check_private(&descriptor, false)?;
            fs::remove_file(&descriptor)?;
        }
        Ok(Self {
            directory,
            descriptor,
            _lock: lock,
            token,
        })
    }

    pub fn publish(&self, endpoint: String) -> io::Result<()> {
        let temporary = self
            .directory
            .join(format!(".connection-{}", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(
                &mut file,
                &RuntimeConnection {
                    endpoint,
                    token: self.token.clone(),
                },
            )?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.descriptor)?;
            File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.descriptor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_rotating_descriptor_and_singleton_lock() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::acquire(temp.path()).unwrap();
        assert!(Runtime::acquire(temp.path()).is_err());
        runtime.publish("http://127.0.0.1:1234/v1".into()).unwrap();
        let descriptor = temp.path().join("ene-host/connection.json");
        assert_eq!(
            fs::metadata(&descriptor).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let first: RuntimeConnection =
            serde_json::from_slice(&fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(first.token, runtime.token);
        drop(runtime);
        assert!(!descriptor.exists());
        let second = Runtime::acquire(temp.path()).unwrap();
        assert_ne!(first.token, second.token);
    }
    #[test]
    fn public_or_symlinked_runtime_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Runtime::acquire(temp.path()).is_err());
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), temp.path().join("ene-host")).unwrap();
        assert!(Runtime::acquire(temp.path()).is_err());
    }
}
