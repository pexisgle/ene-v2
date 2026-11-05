//! Temporary provider bundles must satisfy the same ownership checks as installed code.
use std::{io, path::Path};

fn private_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

pub(crate) fn create_directory(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    std::fs::create_dir_all(path)?;
    private_mode(path, 0o700)
}

pub(crate) fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        create_directory(parent)?;
    }
    std::fs::write(path, bytes)?;
    private_mode(path, 0o600)
}
