pub use ene_platform::storage::{private_directory, private_file};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};
pub struct Runtime {
    _directory: File,
    _lock: File,
    descriptor: PathBuf,
    pub token: String,
}
impl Runtime {
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let _root = ene_platform::storage::open(root, true, true)?;
        let directory = root.join("ene-host");
        let handle = private_directory(&directory)?;
        let lock = private_file(&directory.join("host.lock"))?;
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
        let descriptor = directory.join("connection.json");
        if descriptor.exists() {
            ene_platform::storage::check(&descriptor, false, true)?;
            fs::remove_file(&descriptor)?;
        }
        Ok(Self {
            _directory: handle,
            _lock: lock,
            descriptor,
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        })
    }
    pub fn publish(&self, endpoint: String) -> io::Result<()> {
        let mut bytes =
            serde_json::to_vec(&serde_json::json!({"endpoint":endpoint,"token":self.token}))?;
        bytes.push(b'\n');
        ene_platform::storage::write_atomic(&self.descriptor, &bytes)
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
    fn singleton_and_descriptor_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        private_directory(&root).unwrap();
        let runtime = Runtime::acquire(&root).unwrap();
        assert!(Runtime::acquire(&root).is_err());
        runtime.publish("http://127.0.0.1:1234/v1".into()).unwrap();
        let path = root.join("ene-host/connection.json");
        assert!(
            !ene_platform::storage::read(&path, 8192, true)
                .unwrap()
                .is_empty()
        );
        drop(runtime);
        assert!(!path.exists());
        Runtime::acquire(&root).unwrap();
    }
}
