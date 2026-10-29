use std::{io, path::PathBuf};

#[cfg(windows)]
fn home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .expect("LOCALAPPDATA must name an absolute user directory")
        .join("ene")
}
pub(crate) fn state() -> PathBuf {
    #[cfg(windows)]
    {
        home().join("state")
    }
    #[cfg(unix)]
    {
        xdg::BaseDirectories::new()
            .get_state_home()
            .expect("XDG state home")
            .join("ene")
    }
}
pub(crate) fn data() -> PathBuf {
    #[cfg(windows)]
    {
        home()
    }
    #[cfg(unix)]
    {
        xdg::BaseDirectories::new()
            .get_data_home()
            .expect("XDG data home")
            .join("ene")
    }
}
pub(crate) fn runtime() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let home = home();
        let root = home.join("runtime");
        ene_platform::storage::create_private_dir_all(&root)?;
        Ok(root)
    }
    #[cfg(unix)]
    {
        xdg::BaseDirectories::new()
            .get_runtime_directory()
            .cloned()
            .map_err(io::Error::other)
    }
}
