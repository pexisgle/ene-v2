//! Private runtime directory, singleton lock and connection descriptor.
use cap_fs_ext::{
    DirExt, FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt,
};
use cap_std::{
    ambient_authority,
    fs::{Dir, File as CapFile, OpenOptions},
};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
};
use uuid::Uuid;

pub struct Runtime {
    directory: Dir,
    _lock: File,
    pub token: String,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe ene file ownership, mode or type",
    )
}

/// Open a final path component without following a symlink or reparse point.
fn open_ambient_dir(path: &Path) -> io::Result<Dir> {
    let Some(name) = path.file_name() else {
        return Dir::open_ambient_dir(path, ambient_authority());
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Dir::open_ambient_dir(parent, ambient_authority())?.open_dir_nofollow(name)
}

fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn check_private_dir(dir: &Dir) -> io::Result<()> {
    if !dir.dir_metadata()?.is_dir() {
        return Err(denied());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = dir.try_clone()?.into_std_file().metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(denied());
        }
    }
    #[cfg(windows)]
    if !reparse_free(&dir.try_clone()?.into_std_file().metadata()?) {
        return Err(denied());
    }
    Ok(())
}

fn check_private_file(file: &CapFile) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || CapMetadataExt::nlink(&metadata) != 1 {
        return Err(denied());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.try_clone()?.into_std().metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(denied());
        }
    }
    #[cfg(windows)]
    if !reparse_free(&file.try_clone()?.into_std().metadata()?) {
        return Err(denied());
    }
    Ok(())
}

#[cfg(windows)]
fn reparse_free(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

/// Create (if missing) and open a private directory.
pub fn private_directory(path: &Path) -> io::Result<Dir> {
    create_private_dir_all(path)?;
    let dir = open_ambient_dir(path)?;
    check_private_dir(&dir)?;
    Ok(dir)
}

fn open_private_directory(path: &Path) -> io::Result<Dir> {
    let dir = open_ambient_dir(path)?;
    check_private_dir(&dir)?;
    Ok(dir)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

fn open_private_file(directory: &Dir, name: impl AsRef<Path>) -> io::Result<CapFile> {
    let file = directory.open_with(name, &private_options())?;
    check_private_file(&file)?;
    Ok(file)
}

fn open_existing_file(directory: &Dir, name: &str) -> io::Result<CapFile> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = directory.open_with(name, &options)?;
    check_private_file(&file)?;
    Ok(file)
}

/// Create an empty private file or reopen an existing one.
pub fn private_file(path: &Path) -> io::Result<CapFile> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let directory = open_private_directory(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing name"))?;
    open_private_file(&directory, name)
}

fn write_atomic(directory: &Dir, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = format!(".ene-{}", Uuid::new_v4());
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = directory.open_with(&temporary, &options)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        directory.rename(&temporary, directory, name)?;
        // ponytail: cap-std keeps directory handles as O_PATH on Linux, so a
        // parent fsync is unavailable. The previous implementation did not
        // fsync the parent either; reopen by path if rename durability matters.
        Ok(())
    })();
    let _ = directory.remove_file(&temporary);
    result
}

#[cfg(unix)]
fn directory_options() -> cap_std::fs::DirBuilder {
    use cap_std::fs::DirBuilderExt as _;
    let mut builder = cap_std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn directory_options() -> cap_std::fs::DirBuilder {
    cap_std::fs::DirBuilder::new()
}

impl Runtime {
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let root = open_private_directory(root)?;
        match root.create_dir_with("ene-host", &directory_options()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = root.open_dir_nofollow("ene-host")?;
        check_private_dir(&directory)?;
        let lock = open_private_file(&directory, "host.lock")?.into_std();
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
        match open_existing_file(&directory, "connection.json") {
            Ok(file) => {
                drop(file);
                directory.remove_file("connection.json")?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            directory,
            _lock: lock,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        })
    }

    pub fn publish(&self, endpoint: String) -> io::Result<()> {
        let mut bytes =
            serde_json::to_vec(&serde_json::json!({"endpoint":endpoint,"token":self.token}))?;
        bytes.push(b'\n');
        write_atomic(&self.directory, "connection.json", &bytes)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.directory.remove_file("connection.json");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_temp() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        temp
    }

    #[test]
    fn singleton_publish_and_cleanup() {
        let temp = private_temp();
        let runtime = Runtime::acquire(temp.path()).unwrap();
        assert!(Runtime::acquire(temp.path()).is_err());
        runtime.publish("http://127.0.0.1:1234/v1".into()).unwrap();
        let path = temp.path().join("ene-host/connection.json");
        let descriptor: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(descriptor["endpoint"], "http://127.0.0.1:1234/v1");
        assert_eq!(descriptor["token"], runtime.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        drop(runtime);
        assert!(!path.exists());
        Runtime::acquire(temp.path()).unwrap();
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn private_directories_and_files_reject_public_modes_and_hardlinks() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let public = temp.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&public).is_err());

        let root = temp.path().join("private");
        let directory = private_directory(&root).unwrap();
        drop(open_private_file(&directory, "connection.json").unwrap());
        fs::hard_link(root.join("connection.json"), root.join("link")).unwrap();
        assert!(open_existing_file(&directory, "connection.json").is_err());
        assert!(private_file(&root.join("connection.json")).is_err());
    }

    #[test]
    fn write_atomic_replaces_utf8_contents() {
        let temp = tempfile::tempdir().unwrap();
        let directory = private_directory(&temp.path().join("private")).unwrap();
        write_atomic(&directory, "text", b"old").unwrap();
        write_atomic(&directory, "text", "日本語🙂".as_bytes()).unwrap();
        assert_eq!(directory.read("text").unwrap(), "日本語🙂".as_bytes());
    }
}
