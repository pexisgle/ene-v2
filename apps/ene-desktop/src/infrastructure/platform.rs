//! Resolve this client's local configuration and asset locations.
use std::path::PathBuf;

pub fn config() -> PathBuf {
    xdg::BaseDirectories::new()
        .get_config_home()
        .expect("XDG config home is unavailable")
        .join("ene")
}

pub fn cache() -> PathBuf {
    xdg::BaseDirectories::new()
        .get_cache_home()
        .expect("XDG cache home is unavailable")
        .join("ene")
}
