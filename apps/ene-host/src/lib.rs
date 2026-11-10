//! The ene host: one headless Rig app, one SQLite connection and a small
//! Connect API.

mod host;
mod server;

pub use server::run;

/// State and runtime directories, with explicit environment overrides.
///
/// `ENE_HOST_STATE_DIR` overrides the SQLite/rig directory.
/// `ENE_HOST_RUNTIME_DIR` overrides the discovery/lock directory.
pub fn paths() -> std::io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::{env, io, path::PathBuf};
    let defaults = directories::ProjectDirs::from("", "", "ene");
    let state = env::var_os("ENE_HOST_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = defaults.as_ref()?;
            Some(dirs.state_dir().unwrap_or(dirs.data_local_dir()).to_owned())
        })
        .ok_or_else(|| io::Error::other("state directory is not configured"))?;
    let runtime = env::var_os("ENE_HOST_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = defaults.as_ref()?;
            Some(
                dirs.runtime_dir()
                    .map_or_else(|| dirs.cache_dir().join("runtime"), PathBuf::from),
            )
        })
        .ok_or_else(|| io::Error::other("runtime directory is not configured"))?;
    Ok((state, runtime))
}

/// Truncates `value` to at most `limit` Unicode scalar values.
pub(crate) fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn truncation_keeps_the_character_limit() {
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("あいう", 2), "あい");
        assert_eq!(truncate_chars("hello", 0), "");
    }
}
