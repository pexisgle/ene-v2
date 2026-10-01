//! XDG locations shared by clients and the local host.
use std::path::PathBuf;

fn directories() -> xdg::BaseDirectories {
    xdg::BaseDirectories::new()
}
pub fn config() -> PathBuf {
    directories()
        .get_config_home()
        .expect("XDG config home is unavailable")
        .join("ene")
}
pub fn cache() -> PathBuf {
    directories()
        .get_cache_home()
        .expect("XDG cache home is unavailable")
        .join("ene")
}
pub fn state() -> PathBuf {
    directories()
        .get_state_home()
        .expect("XDG state home is unavailable")
        .join("ene")
}
pub fn runtime() -> Result<PathBuf, xdg::BaseDirectoriesError> {
    directories().get_runtime_directory().cloned()
}
