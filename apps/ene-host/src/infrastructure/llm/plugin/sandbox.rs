#[cfg(unix)]
#[path = "sandbox_unix.rs"]
mod platform;
#[cfg(windows)]
pub use ene_platform::sandbox::Child;
#[cfg(unix)]
pub use platform::*;
#[cfg(unix)]
pub use tokio::process::Child;
#[cfg(windows)]
pub struct Spawned {
    pub child: Child,
    pub image: tempfile::TempDir,
}
#[cfg(windows)]
pub fn spawn(bundle: &super::manifest::Bundle) -> ene_core::ports::Result<Spawned> {
    use ene_core::domain::{Error, ErrorKind};
    if super::manifest::load(&bundle.root)?.revision != bundle.revision {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let image = tempfile::tempdir().map_err(|_| Error::unavailable())?;
    for file in bundle.files.iter() {
        let path = image.path().join(&file.path);
        if let Some(bytes) = &file.bytes {
            if let Some(parent) = path.parent() {
                ene_platform::storage::create_private_dir_all(parent)
                    .map_err(|_| Error::unavailable())?;
            }
            ene_platform::storage::write_atomic(&path, bytes).map_err(|_| Error::unavailable())?;
        } else {
            ene_platform::storage::create_private_dir_all(&path)
                .map_err(|_| Error::unavailable())?;
        }
    }
    let child = ene_platform::sandbox::spawn(image.path(), &bundle.manifest.command, None)
        .map_err(|_| Error::unavailable())?;
    Ok(Spawned { child, image })
}
