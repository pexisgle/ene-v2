use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Debug, Parser)]
#[command(version, about = "Serve the authenticated ene API")]
pub struct HostArgs {
    #[arg(long, help = "Clear learned history while the host is stopped")]
    pub(crate) reset_history: bool,
    #[arg(long, env = "ENE_HOST_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[arg(long, env = "ENE_HOST_RUNTIME_DIR")]
    runtime_dir: Option<PathBuf>,
    #[arg(long, env = "ENE_DEV_HOST_DIR", hide = true)]
    dev_host_dir: Option<PathBuf>,
    #[arg(long, env = "ENE_HOST_BIND", default_value = "127.0.0.1:0")]
    pub(crate) bind: SocketAddr,
    #[arg(long, env = "ENE_HOST_ALLOWED_HOSTS", default_value = "")]
    pub(crate) allowed_hosts: String,
}

impl HostArgs {
    pub(crate) fn paths(&self) -> Result<(PathBuf, PathBuf)> {
        let defaults = ProjectDirs::from("", "", "ene");
        let state = self
            .state_dir
            .clone()
            .or_else(|| self.dev_host_dir.as_ref().map(|dir| dir.join("state")))
            .or_else(|| {
                defaults
                    .as_ref()
                    .map(|dirs| dirs.state_dir().unwrap_or(dirs.data_local_dir()).to_owned())
            })
            .context("set ENE_HOST_STATE_DIR to a writable path")?;
        let runtime = self
            .runtime_dir
            .clone()
            .or_else(|| self.dev_host_dir.as_ref().map(|dir| dir.join("runtime")))
            .or_else(|| {
                defaults.as_ref().map(|dirs| {
                    dirs.runtime_dir().map_or_else(
                        || dirs.cache_dir().join("runtime"),
                        std::path::Path::to_owned,
                    )
                })
            })
            .context("set ENE_HOST_RUNTIME_DIR to a writable path")?;
        Ok((state, runtime))
    }
}
