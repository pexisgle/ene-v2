use std::path::PathBuf;
pub fn runtime() -> Result<PathBuf, xdg::BaseDirectoriesError> {
    xdg::BaseDirectories::new().get_runtime_directory().cloned()
}
pub fn state() -> PathBuf {
    xdg::BaseDirectories::new()
        .get_state_home()
        .expect("XDG state home is unavailable")
        .join("ene")
}
