//! Temporary provider bundles must satisfy the same ACL checks as installed code.
use std::{io, path::Path};

pub(crate) fn create_directory(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::create_dir(path)
    }
    #[cfg(windows)]
    {
        ene_platform::storage::create_private_dir_all(path.as_ref())
    }
}

pub(crate) fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::write(path, bytes)
    }
    #[cfg(windows)]
    {
        ene_platform::storage::write_atomic(path.as_ref(), bytes.as_ref())
    }
}

pub(crate) fn package_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    {
        // Recreate this fresh, empty fixture through the private storage API.
        // Keep TempDir's ownership so all contents are removed after the test.
        std::fs::remove_dir(directory.path()).unwrap();
        ene_platform::storage::create_private_dir_all(directory.path()).unwrap();
    }
    directory
}
