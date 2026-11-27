//! The desktop client's view of one host: local loopback discovery or an
//! explicit remote HTTPS ingress, and the generated Connect RPC clients.
//!
//! For local hosts, the host publishes `connection.json` into its runtime
//! directory (`http://<loopback>/rpc` + Owner Bearer, validated by
//! [`ene_rpc::connect`]). For remote hosts, the user explicitly supplies an
//! `https://<host>/rpc` ingress and Bearer token validated by
//! [`ene_rpc::connect_remote`]; plaintext remote URLs are always rejected.

use connectrpc::client::CallOptions;
use ene_rpc as w;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Default deadline for desktop requests with one response.
pub(crate) const RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn unary_options() -> CallOptions {
    CallOptions::default().with_timeout(RPC_TIMEOUT)
}

/// What the host publishes for a client to connect with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    pub endpoint: String,
    pub token: String,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery").finish_non_exhaustive()
    }
}

/// Selected host target: local discovery file or explicit remote HTTPS host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum HostTarget {
    Local { path: PathBuf },
    Remote { endpoint: String, token: String },
}

impl HostTarget {
    /// Validates a remote HTTPS `/rpc` ingress URL.
    pub fn validate_remote_endpoint(endpoint: &str) -> Result<(), Connect> {
        ene_rpc::validate_remote_endpoint(endpoint)
            .map(|_| ())
            .map_err(|error| Connect::Invalid(error.to_string()))
    }

    /// Reads a persisted remote target (`remote_host.json`), returning `Ok(None)`
    /// when not configured.
    pub fn read_remote(path: &Path) -> Result<Option<Self>, Connect> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Connect::Unavailable(format!("{}: {error}", path.display()))),
        };
        let discovery: Discovery = serde_json::from_slice(&bytes)
            .map_err(|error| Connect::Invalid(format!("{}: {error}", path.display())))?;
        ene_rpc::validate_remote_endpoint(&discovery.endpoint)
            .map_err(|error| Connect::Invalid(error.to_string()))?;
        Ok(Some(Self::Remote {
            endpoint: discovery.endpoint,
            token: discovery.token,
        }))
    }

    /// Validates and persists a remote HTTPS target (`remote_host.json`).
    pub fn write_remote(path: &Path, endpoint: &str, token: &str) -> Result<(), Connect> {
        ene_rpc::validate_remote_endpoint(endpoint)
            .map_err(|error| Connect::Invalid(error.to_string()))?;
        if token.is_empty() || token.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
            return Err(Connect::Invalid("invalid Bearer token".into()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Connect::Unavailable(format!("{}: {error}", parent.display())))?;
        }
        let body = serde_json::to_vec_pretty(&Discovery {
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
        })
        .map_err(|error| Connect::Invalid(error.to_string()))?;
        std::fs::write(path, body)
            .map_err(|error| Connect::Unavailable(format!("{}: {error}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _discarded = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

impl Discovery {
    /// Reads the host's discovery file.
    pub fn read(path: &Path) -> Result<Self, Connect> {
        let bytes = std::fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                // The host has not started, or has not published yet. That is a
                // state the window shows, not a failure to retry blindly.
                Connect::Missing(path.display().to_string())
            } else {
                Connect::Unavailable(format!("{}: {error}", path.display()))
            }
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| Connect::Invalid(format!("{}: {error}", path.display())))
    }
}

/// Why the client could not reach or bind to the host.
#[derive(Debug, thiserror::Error)]
pub enum Connect {
    /// The platform could not provide a configuration directory.
    #[error("設定ディレクトリを特定できませんでした。")]
    MissingConfigDir,
    /// No discovery file yet.
    #[error("the host has not published a connection: {0}")]
    Missing(String),
    /// The file exists but the client cannot use it.
    #[error("the published connection is unusable: {0}")]
    Invalid(String),
    /// Reading the file failed for another reason.
    #[error("the host is not reachable: {0}")]
    Unavailable(String),
}

/// The generated clients over one transport. Cloning a client is cheap and
/// shares the connection pool, so each request takes its own copy.
#[derive(Clone)]
pub struct Host {
    pub host: w::HostServiceClient<connectrpc::client::HttpClient>,
    pub chat: w::ChatServiceClient<connectrpc::client::HttpClient>,
    pub settings: w::SettingsServiceClient<connectrpc::client::HttpClient>,
    pub voice: w::VoiceServiceClient<connectrpc::client::HttpClient>,
    pub work: w::WorkServiceClient<connectrpc::client::HttpClient>,
    pub client: w::ClientServiceClient<connectrpc::client::HttpClient>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl Host {
    fn from_parts(
        transport: connectrpc::client::HttpClient,
        config: connectrpc::client::ClientConfig,
    ) -> Self {
        Self {
            host: w::HostServiceClient::new(transport.clone(), config.clone()),
            chat: w::ChatServiceClient::new(transport.clone(), config.clone()),
            settings: w::SettingsServiceClient::new(transport.clone(), config.clone()),
            voice: w::VoiceServiceClient::new(transport.clone(), config.clone()),
            work: w::WorkServiceClient::new(transport.clone(), config.clone()),
            client: w::ClientServiceClient::new(transport, config),
        }
    }

    /// Connects to a published loopback endpoint. The endpoint and token are
    /// validated by `ene_rpc::connect`; nothing here widens that.
    pub fn connect(discovery: &Discovery) -> Result<Self, Connect> {
        let (transport, config) = ene_rpc::connect(&discovery.endpoint, &discovery.token)
            .map_err(|error| Connect::Invalid(error.to_string()))?;
        Ok(Self::from_parts(transport, config))
    }

    /// A file selected by the operator may identify a remote HTTPS ingress.
    pub(crate) fn connect_explicit(discovery: &Discovery) -> Result<Self, Connect> {
        if discovery
            .endpoint
            .split_once(':')
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("https"))
        {
            Self::connect_remote(&discovery.endpoint, &discovery.token)
        } else {
            Self::connect(discovery)
        }
    }

    /// Connects to an explicitly configured remote HTTPS ingress using TLS +
    /// Bearer authentication (`ene_rpc::connect_remote`).
    // Exercised by unit tests and reserved for remote ingress UI; remove once wired to UI.
    pub fn connect_remote(endpoint: &str, token: &str) -> Result<Self, Connect> {
        let (transport, config) = ene_rpc::connect_remote(endpoint, token)
            .map_err(|error| Connect::Invalid(error.to_string()))?;
        Ok(Self::from_parts(transport, config))
    }

    /// Connects according to a local or remote [`HostTarget`].
    // Exercised by unit tests and reserved for remote ingress UI; remove once wired to UI.
    pub fn connect_target(target: &HostTarget) -> Result<Self, Connect> {
        match target {
            HostTarget::Local { path } => Self::connect(&Discovery::read(path)?),
            HostTarget::Remote { endpoint, token } => Self::connect_remote(endpoint, token),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_its_own_state_and_not_an_invalid_file() {
        let path = std::path::Path::new("no-such-connection.json");
        match Discovery::read(path) {
            Err(Connect::Missing(_)) => {}
            other => panic!("a missing file is Missing, got {other:?}"),
        }
    }

    #[test]
    fn a_file_that_is_not_the_published_shape_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connection.json");
        std::fs::write(&path, b"{\"endpoint\": 1}").unwrap();
        match Discovery::read(&path) {
            Err(Connect::Invalid(_)) => {}
            other => panic!("a malformed file is Invalid, got {other:?}"),
        }
    }

    #[test]
    fn f10_remote_host_target_enforces_https_and_roundtrips_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote_host.json");
        assert!(HostTarget::read_remote(&path).unwrap().is_none());

        assert!(matches!(
            HostTarget::write_remote(&path, "http://ene.example.com/rpc", "tok"),
            Err(Connect::Invalid(_))
        ));
        assert!(matches!(
            Host::connect_remote("http://ene.example.com/rpc", "tok"),
            Err(Connect::Invalid(_))
        ));

        HostTarget::write_remote(&path, "https://ene.example.com/rpc", "remote-bearer-token")
            .unwrap();
        let loaded = HostTarget::read_remote(&path).unwrap().unwrap();
        assert_eq!(
            loaded,
            HostTarget::Remote {
                endpoint: "https://ene.example.com/rpc".into(),
                token: "remote-bearer-token".into(),
            }
        );
        assert!(Host::connect_target(&loaded).is_ok());
        let fields = Discovery::read(&path).unwrap();
        assert!(Host::connect(&fields).is_err());
        assert!(Host::connect_explicit(&fields).is_ok());
        assert!(!format!("{fields:?}").contains(&fields.token));
    }
}
