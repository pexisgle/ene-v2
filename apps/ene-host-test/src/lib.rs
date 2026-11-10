//! A small persistent Rig ECS host and Connect client, for host experiments.

mod host;

pub mod client;
pub mod proto {
    connectrpc::include_generated!();
}
pub mod server;

/// State and runtime paths shared by the host and client, with explicit env overrides.
/// Uses `ProjectDirs::state_dir` or local data, and runtime or `cache/runtime`.
/// Directory privacy is enforced when the host opens them with `private-fs`.
pub fn paths() -> std::io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::{env, io, path::PathBuf};
    let defaults = directories::ProjectDirs::from("", "", "ene-host-test");
    let state = env::var_os("ENE_HOST_TEST_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = defaults.as_ref()?;
            Some(dirs.state_dir().unwrap_or(dirs.data_local_dir()).to_owned())
        })
        .ok_or_else(|| io::Error::other("state directory is not configured"))?;
    let runtime = env::var_os("ENE_HOST_TEST_RUNTIME_DIR")
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
