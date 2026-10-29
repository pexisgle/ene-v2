use std::{io, path::PathBuf};

#[cfg(windows)]
fn home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .expect("LOCALAPPDATA must name an absolute user directory")
        .join("ene")
}
pub fn config() -> PathBuf {
    #[cfg(windows)]
    {
        home().join("config")
    }
    #[cfg(unix)]
    {
        xdg::BaseDirectories::new()
            .get_config_home()
            .expect("XDG config home")
            .join("ene")
    }
}
pub fn cache() -> PathBuf {
    #[cfg(windows)]
    {
        home().join("cache")
    }
    #[cfg(unix)]
    {
        xdg::BaseDirectories::new()
            .get_cache_home()
            .expect("XDG cache home")
            .join("ene")
    }
}
pub fn state() -> PathBuf {
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
pub fn data() -> PathBuf {
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
pub fn runtime() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let home = home();
        let root = home.join("runtime");
        super::storage::create_private_dir_all(&root)?;
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
pub fn providers() -> PathBuf {
    data().join("providers")
}
pub fn plugins() -> PathBuf {
    data().join("plugins")
}
