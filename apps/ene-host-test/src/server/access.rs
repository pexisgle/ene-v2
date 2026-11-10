//! Private discovery; native permissions and atomic publication belong to private-fs.
use io_lifetimes::AsFilelike;
use private_fs::{Durability, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(super) struct Access {
    _locks: [PrivateFile; 2],
    directory: PrivateDir,
    pub token: String,
    pub connection: PathBuf,
}

impl Access {
    pub(super) fn acquire(state: &Path, runtime: &Path) -> Result<Self> {
        for path in [state, runtime] {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
        }
        let state = PrivateDir::ensure(state)?;
        let directory = PrivateDir::ensure(runtime)?;
        let options = OpenOptions {
            access: FileAccess::ReadWrite,
            creation: FileCreation::OpenOrCreate,
            ..Default::default()
        };
        let locks = [
            state.open_file_with("host.lock", options)?,
            directory.open_file_with("host.lock", options)?,
        ];
        for lock in &locks {
            lock.as_filelike_view::<File>()
                .try_lock()
                .map_err(|_| io::Error::other("state or runtime directory already in use"))?;
        }
        Ok(Self {
            _locks: locks,
            directory,
            token: format!("{}{}", Uuid::new_v4(), Uuid::new_v4()),
            connection: runtime.join("connection.json"),
        })
    }

    pub(super) fn publish(&self, endpoint: &str) -> Result<()> {
        self.directory.write_atomic_durable(
            "connection.json",
            &serde_json::to_vec(&serde_json::json!({"endpoint": endpoint, "token": self.token}))?,
        )?;
        Ok(())
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        let _ = self
            .directory
            .remove_file("connection.json", Durability::None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_private_exclusive_rotates_and_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let runtime = root.join("runtime");
        let first = Access::acquire(&state, &runtime).unwrap();
        first.publish("http://127.0.0.1:1234").unwrap();
        private_fs::verify(&File::open(&first.connection).unwrap()).unwrap();
        let connection_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&first.connection).unwrap()).unwrap();
        assert_eq!(connection_json["token"], first.token);
        first.publish("http://127.0.0.1:4321").unwrap();
        let connection_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&first.connection).unwrap()).unwrap();
        assert_eq!(connection_json["endpoint"], "http://127.0.0.1:4321");
        assert!(Access::acquire(&state, &root.join("other-runtime")).is_err());
        assert!(Access::acquire(&root.join("other-state"), &runtime).is_err());
        let token = first.token.clone();
        let connection = first.connection.clone();
        drop(first);
        assert!(!connection.exists());
        let second = Access::acquire(&state, &runtime).unwrap();
        assert_ne!(token, second.token);
    }

    #[test]
    fn hardlinked_lock_is_rejected_without_changing_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let directory = PrivateDir::ensure(&state).unwrap();
        directory.create("host.lock").unwrap();
        fs::hard_link(state.join("host.lock"), root.join("alias")).unwrap();
        assert!(Access::acquire(&state, &root.join("runtime")).is_err());
        assert!(root.join("alias").exists());
    }

    #[test]
    #[cfg(unix)]
    fn exposed_directories_and_symlinked_locks_are_rejected_without_repair() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let runtime = root.join("runtime");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Access::acquire(&state, &runtime).is_err());
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("outside"), "unchanged").unwrap();
        symlink(root.join("outside"), state.join("host.lock")).unwrap();
        assert!(Access::acquire(&state, &runtime).is_err());
        assert_eq!(
            fs::read_to_string(root.join("outside")).unwrap(),
            "unchanged"
        );
    }
}
