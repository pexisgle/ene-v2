//! Command-line and environment settings for the host process.

use clap::{
    Parser,
    builder::{OsStringValueParser, TypedValueParser as _},
};
use directories::ProjectDirs;
use std::{
    ffi::OsString,
    net::{AddrParseError, SocketAddr},
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(name = "ene-host", version, about = "Serve the authenticated ene API")]
pub struct HostArgs {
    /// Clear conversation, work history and memory while the host is stopped.
    #[arg(long)]
    pub reset_history: bool,
    /// Directory for SQLite and the Rig session log.
    #[arg(long, env = "ENE_HOST_STATE_DIR")]
    state_dir: Option<OsString>,
    /// Directory for the discovery file and host lock.
    #[arg(long, env = "ENE_HOST_RUNTIME_DIR")]
    runtime_dir: Option<OsString>,
    #[arg(long, env = "ENE_DEV_HOST_DIR", hide = true)]
    dev_host_dir: Option<OsString>,
    /// Socket address to listen on.
    #[arg(long, env = "ENE_HOST_BIND", default_value = "127.0.0.1:0", value_parser = bind_address)]
    pub(crate) bind: SocketAddr,
    /// Comma-separated authorities trusted in addition to loopback.
    #[arg(long, env = "ENE_HOST_ALLOWED_HOSTS", default_value = "")]
    pub(crate) allowed_hosts: String,
    /// Exit when the desktop closes the host's stdin.
    #[arg(
        long, env = "ENE_HOST_EXIT_ON_STDIN_EOF", action = clap::ArgAction::Set,
        num_args = 0..=1, default_value = "0", default_missing_value = "1",
        value_parser = OsStringValueParser::new().map(|value| !value.is_empty() && value != "0")
    )]
    pub(crate) exit_on_stdin_eof: bool,
}

impl HostArgs {
    pub(crate) fn paths(&self) -> Result<(PathBuf, PathBuf), MissingDirectory> {
        let defaults = ProjectDirs::from("", "", "ene");
        let dev_dir = configured_path(self.dev_host_dir.as_deref());
        let state = configured_path(self.state_dir.as_deref())
            .or_else(|| dev_dir.as_ref().map(|dir| dir.join("state")))
            .or_else(|| {
                defaults
                    .as_ref()
                    .map(|dirs| dirs.state_dir().unwrap_or(dirs.data_local_dir()).to_owned())
            })
            .ok_or(MissingDirectory("ENE_HOST_STATE_DIR"))?;
        let runtime = configured_path(self.runtime_dir.as_deref())
            .or_else(|| dev_dir.map(|dir| dir.join("runtime")))
            .or_else(|| {
                defaults.as_ref().map(|dirs| {
                    dirs.runtime_dir()
                        .map_or_else(|| dirs.cache_dir().join("runtime"), Path::to_path_buf)
                })
            })
            .ok_or(MissingDirectory("ENE_HOST_RUNTIME_DIR"))?;
        Ok((state, runtime))
    }
}

fn configured_path(value: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn bind_address(value: &str) -> Result<SocketAddr, AddrParseError> {
    match value.trim() {
        "" => Ok(SocketAddr::from(([127, 0, 0, 1], 0))),
        text => text.parse(),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("set {0} to a writable directory")]
pub(crate) struct MissingDirectory(&'static str);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_accepts_a_socket_address_and_keeps_blank_as_loopback() {
        assert_eq!(
            bind_address("  ").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 0))
        );
        assert_eq!(
            bind_address(" 0.0.0.0:8080 ").unwrap().to_string(),
            "0.0.0.0:8080"
        );
        assert!(bind_address("localhost:9").is_err());
    }

    #[test]
    fn development_paths_fill_in_missing_or_empty_overrides() {
        let args = HostArgs::try_parse_from([
            "ene-host",
            "--dev-host-dir",
            "dev",
            "--state-dir",
            "",
            "--runtime-dir",
            "custom-runtime",
        ])
        .unwrap();
        assert_eq!(
            args.paths().unwrap(),
            ("dev/state".into(), "custom-runtime".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_line_keeps_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = OsString::from_vec(b"/tmp/ene-\xFF-state".to_vec());
        let config = HostArgs::try_parse_from([
            "ene-host".into(),
            "--state-dir".into(),
            raw.clone(),
            "--runtime-dir".into(),
            "/tmp/ene-runtime".into(),
        ])
        .unwrap();
        assert_eq!(config.paths().unwrap().0.as_os_str(), raw.as_os_str());
    }
}
