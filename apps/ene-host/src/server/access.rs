//! Private discovery and single-host exclusion.
//!
//! Directory permissions, atomic publication and lock checks belong to
//! `private-fs`; the host only supplies the paths and token.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use io_lifetimes::AsFilelike;
use private_fs::{Durability, FileAccess, FileCreation, OpenOptions, PrivateDir, PrivateFile};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    fs::{self, File},
    io,
    path::Path,
};

use anyhow::Result;

pub(super) struct Access {
    _locks: [PrivateFile; 2],
    directory: PrivateDir,
    /// The discovery file the tests read back; production code reaches the
    /// same path through `directory` and `publish`.
    #[cfg(test)]
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
            #[cfg(test)]
            connection: runtime.join("connection.json"),
        })
    }

    pub(super) fn publish(&self, endpoint: &str) -> Result<String> {
        let mut token = [0u8; 32];
        getrandom::fill(&mut token)?;
        let token = URL_SAFE_NO_PAD.encode(token);
        self.directory.write_atomic_durable(
            "connection.json",
            &serde_json::to_vec(&serde_json::json!({"endpoint": endpoint, "token": token}))?,
        )?;
        Ok(token)
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        let _discarded = self
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
        let first_token = first.publish("http://127.0.0.1:1234/rpc").unwrap();
        private_fs::verify(&File::open(&first.connection).unwrap()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&first.connection).unwrap()).unwrap();
        assert_eq!(value["token"], first_token);
        first.publish("http://127.0.0.1:4321/rpc").unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&first.connection).unwrap()).unwrap();
        assert_eq!(value["endpoint"], "http://127.0.0.1:4321/rpc");
        assert!(Access::acquire(&state, &root.join("other-runtime")).is_err());
        assert!(Access::acquire(&root.join("other-state"), &runtime).is_err());
        let connection = first.connection.clone();
        drop(first);
        assert!(!connection.exists());
        let second = Access::acquire(&state, &runtime).unwrap();
        assert_ne!(
            first_token,
            second.publish("http://127.0.0.1:1234/rpc").unwrap()
        );
    }
}
