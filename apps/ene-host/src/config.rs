//! Everything the host reads from its environment, resolved once at startup.
//!
//! Environment variables cross a system boundary, so each one is read exactly
//! here, validated, and handed to the rest of the host as a typed field. No
//! module calls `std::env::var` for a host setting, which means a value cannot
//! change mid-run and a malformed one fails at startup instead of at first use.
//!
//! Path-valued settings stay `PathBuf`. `OsString` is the only type that
//! round-trips a non-UTF-8 path, so a host told to use such a directory must not
//! fail because the setting was funnelled through `String`.

use std::net::SocketAddr;
use std::path::PathBuf;

/// The environment variable names this module owns.
///
/// Named as constants so the doc table, the tests and the readers share one
/// spelling.
pub(crate) const STATE_DIR: &str = "ENE_HOST_STATE_DIR";
pub(crate) const RUNTIME_DIR: &str = "ENE_HOST_RUNTIME_DIR";
const DEV_HOST_DIR: &str = "ENE_DEV_HOST_DIR";
pub(crate) const BIND: &str = "ENE_HOST_BIND";
pub(crate) const ALLOWED_HOSTS: &str = "ENE_HOST_ALLOWED_HOSTS";
pub(crate) const EXIT_ON_STDIN_EOF: &str = "ENE_HOST_EXIT_ON_STDIN_EOF";

/// Why a host setting could not be read.
///
/// A caller can tell "the operator typed a bad address" apart from "this
/// machine has no home directory", which a flattened string cannot express.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// The variable is set but its value cannot be used.
    #[error("{name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
    /// No fallback could be derived for a required directory.
    #[error(
        "{0} is not set and no default could be derived on this platform; \
         set it to a writable directory"
    )]
    NoDefault(&'static str),
}

/// The resolved settings for one host process.
#[derive(Debug)]
pub(crate) struct HostConfig {
    /// Where `host.sqlite3` and the Rig session log live.
    pub(crate) state_dir: PathBuf,
    /// Where `connection.json` and the lock live.
    pub(crate) runtime_dir: PathBuf,
    /// Address to listen on.
    pub(crate) bind: SocketAddr,
    /// Authorities trusted besides loopback.
    pub(crate) allowed_hosts: String,
    /// Exit when the desktop closes the host's stdin.
    pub(crate) exit_on_stdin_eof: bool,
}

impl HostConfig {
    /// Reads the real process environment.
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        Self::load(|name| std::env::var_os(name))
    }

    /// Reads settings through `get`, so a test can supply its own environment.
    ///
    /// `get` returns the raw `OsString`, which is what a path setting needs.
    pub(crate) fn load(
        get: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self, ConfigError> {
        let defaults = directories::ProjectDirs::from("", "", "ene");
        let dev_dir = path_from(&get, DEV_HOST_DIR);
        let state_dir = path_from(&get, STATE_DIR)
            .or_else(|| dev_dir.as_ref().map(|dir| dir.join("state")))
            .or_else(|| {
                defaults.as_ref().map(|dirs| {
                    dirs.state_dir()
                        .unwrap_or_else(|| dirs.data_local_dir())
                        .to_owned()
                })
            });
        let Some(state_dir) = state_dir else {
            return Err(ConfigError::NoDefault(STATE_DIR));
        };
        let runtime_dir = path_from(&get, RUNTIME_DIR)
            .or_else(|| dev_dir.map(|dir| dir.join("runtime")))
            .or_else(|| {
                defaults.as_ref().map(|dirs| {
                    dirs.runtime_dir().map_or_else(
                        || dirs.cache_dir().join("runtime"),
                        std::path::Path::to_path_buf,
                    )
                })
            });
        let Some(runtime_dir) = runtime_dir else {
            return Err(ConfigError::NoDefault(RUNTIME_DIR));
        };
        Ok(Self {
            state_dir,
            runtime_dir,
            bind: bind_from(&get)?,
            allowed_hosts: text_from(&get, ALLOWED_HOSTS),
            exit_on_stdin_eof: flag_from(&get, EXIT_ON_STDIN_EOF),
        })
    }
}

/// A path setting, ignoring an empty value the way an unset one is ignored.
fn path_from(get: &impl Fn(&str) -> Option<std::ffi::OsString>, name: &str) -> Option<PathBuf> {
    get(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `None` when the variable is unset or blank, so an operator can clear a
/// default from the shell without editing a config file.
fn text_from(get: &impl Fn(&str) -> Option<std::ffi::OsString>, name: &str) -> String {
    get(name)
        .and_then(|value| value.into_string().ok())
        .map_or_else(String::new, |text| text.trim().to_owned())
}

/// Presence means true, matching how the rest of the host's flags read.
fn flag_from(get: &impl Fn(&str) -> Option<std::ffi::OsString>, name: &str) -> bool {
    get(name).is_some_and(|value| !value.is_empty() && value != "0")
}

/// Parses the bind address, defaulting to an ephemeral loopback port.
fn bind_from(get: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<SocketAddr, ConfigError> {
    let Some(raw) = get(BIND) else {
        return Ok(SocketAddr::from(([127, 0, 0, 1], 0)));
    };
    let text = raw.into_string().map_err(|_| ConfigError::Invalid {
        name: BIND,
        reason: "the value is not valid UTF-8".into(),
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], 0)));
    }
    trimmed.parse().map_err(|error| ConfigError::Invalid {
        name: BIND,
        reason: format!("{trimmed:?} is not an address:port pair ({error})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn load(pairs: &[(&str, &str)]) -> Result<HostConfig, ConfigError> {
        HostConfig::load(env(pairs))
    }

    #[test]
    fn the_documented_defaults_apply_when_every_variable_is_unset() -> Result<(), ConfigError> {
        let config = load(&[])?;
        assert_eq!(config.bind, SocketAddr::from(([127, 0, 0, 1], 0)));
        assert!(!config.exit_on_stdin_eof);
        assert_eq!(config.allowed_hosts, "");
        Ok(())
    }

    #[test]
    fn an_explicit_directory_overrides_the_derived_default() -> Result<(), ConfigError> {
        let config = load(&[
            (STATE_DIR, "/tmp/ene-state"),
            (RUNTIME_DIR, "/tmp/ene-runtime"),
        ])?;
        assert_eq!(config.state_dir, PathBuf::from("/tmp/ene-state"));
        assert_eq!(config.runtime_dir, PathBuf::from("/tmp/ene-runtime"));
        Ok(())
    }

    #[test]
    fn a_blank_directory_is_treated_as_unset() -> Result<(), ConfigError> {
        let with_blank = load(&[(STATE_DIR, "")])?;
        let with_nothing = load(&[])?;
        assert_eq!(with_blank.state_dir, with_nothing.state_dir);
        Ok(())
    }

    #[test]
    fn development_paths_are_defaults_for_missing_or_empty_overrides() -> Result<(), ConfigError> {
        let base = PathBuf::from("target/run/host");
        let config = load(&[(DEV_HOST_DIR, "target/run/host")])?;
        assert_eq!(config.state_dir, base.join("state"));
        assert_eq!(config.runtime_dir, base.join("runtime"));

        let config = load(&[
            (DEV_HOST_DIR, "target/run/host"),
            (STATE_DIR, ""),
            (RUNTIME_DIR, "custom-runtime"),
        ])?;
        assert_eq!(config.state_dir, base.join("state"));
        assert_eq!(config.runtime_dir, PathBuf::from("custom-runtime"));
        Ok(())
    }

    #[test]
    fn a_malformed_bind_address_names_the_variable() {
        let error = load(&[(BIND, "localhost:9")]).expect_err("a hostname is not a socket address");
        let text = error.to_string();
        assert!(
            text.contains(BIND),
            "the error must name the setting: {text}"
        );
        assert!(
            text.contains("localhost:9"),
            "the error must quote the value: {text}"
        );
    }

    #[test]
    fn an_explicit_bind_address_is_parsed() -> Result<(), ConfigError> {
        let config = load(&[(BIND, "0.0.0.0:8080")])?;
        assert_eq!(config.bind.to_string(), "0.0.0.0:8080");
        Ok(())
    }

    #[test]
    fn a_blank_bind_address_falls_back_to_loopback() -> Result<(), ConfigError> {
        let config = load(&[(BIND, "   ")])?;
        assert_eq!(config.bind, SocketAddr::from(([127, 0, 0, 1], 0)));
        Ok(())
    }

    #[test]
    fn a_flag_reads_presence_and_treats_zero_as_off() -> Result<(), ConfigError> {
        assert!(!load(&[])?.exit_on_stdin_eof);
        assert!(load(&[(EXIT_ON_STDIN_EOF, "1")])?.exit_on_stdin_eof);
        assert!(!load(&[(EXIT_ON_STDIN_EOF, "0")])?.exit_on_stdin_eof);
        assert!(!load(&[(EXIT_ON_STDIN_EOF, "")])?.exit_on_stdin_eof);
        Ok(())
    }

    #[test]
    fn a_non_utf8_path_survives_the_parse() -> Result<(), ConfigError> {
        // `OsString` is the only type that round-trips a non-UTF-8 path; a
        // `String`-typed setting would fail here.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(b"/tmp/ene-\xFF-state".to_vec());
            let config = HostConfig::load(|name| (name == STATE_DIR).then(|| raw.clone()))?;
            assert_eq!(config.state_dir.as_os_str(), raw.as_os_str());
        }
        Ok(())
    }
}
