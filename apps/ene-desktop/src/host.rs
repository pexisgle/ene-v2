//! The desktop client's view of one host: local loopback discovery or an
//! explicit remote HTTPS ingress, and the generated Connect RPC clients.
//!
//! For local hosts, the host publishes `connection.json` into its runtime
//! directory (`http://<loopback>/rpc` + Owner Bearer, validated by
//! [`ene_rpc::connect`]). For remote hosts, the user explicitly supplies an
//! `https://<host>/rpc` ingress and Bearer token validated by
//! [`ene_rpc::connect_remote`]; plaintext remote URLs are always rejected.

pub(crate) mod remote;
pub use remote::RemoteProfile;

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

/// The discovery source selected for this desktop session.
#[derive(Debug)]
pub enum HostTarget {
    Discovery {
        path: PathBuf,
        explicit: bool,
    },
    Remote {
        profile: RemoteProfile,
        connected: Option<Box<Host>>,
    },
    Unavailable {
        reason: String,
    },
}

impl HostTarget {
    pub fn saved_remote() -> Option<Self> {
        match RemoteProfile::read() {
            Ok(Some(profile)) => Some(Self::Remote {
                profile,
                connected: None,
            }),
            Ok(None) | Err(Connect::MissingConfigDir) => None,
            Err(error) => Some(Self::Unavailable {
                reason: error.to_string(),
            }),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Discovery { path, .. } => path.display().to_string(),
            Self::Remote { profile, .. } => profile.endpoint.clone(),
            Self::Unavailable { .. } => "遠隔ホスト".into(),
        }
    }

    pub(crate) async fn connect(&mut self) -> Result<Host, Connect> {
        match self {
            Self::Discovery { path, explicit } => {
                let discovery = Discovery::read(path)?;
                if *explicit {
                    Host::connect_explicit(&discovery)
                } else {
                    Host::connect(&discovery)
                }
            }
            Self::Remote { profile, connected } => {
                if let Some(host) = connected {
                    return Ok(host.as_ref().clone());
                }
                let profile = profile.clone();
                let host = tokio::task::spawn_blocking(move || {
                    let token = profile.with_token()?;
                    Host::connect_remote(&profile.endpoint, &token)
                })
                .await
                .map_err(|error| Connect::Unavailable(error.to_string()))??;
                *connected = Some(Box::new(host.clone()));
                Ok(host)
            }
            Self::Unavailable { reason } => Err(Connect::Invalid(reason.clone())),
        }
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

    pub(crate) fn endpoint(&self) -> String {
        self.host.config().base_uri().to_string()
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
    fn f10_explicit_discovery_enforces_https_for_remote_hosts() {
        let mut discovery = Discovery {
            endpoint: "http://ene.example.com/rpc".into(),
            token: "synthetic-remote-bearer".into(),
        };
        assert!(Host::connect_explicit(&discovery).is_err());
        discovery.endpoint = "https://ene.example.com/rpc".into();
        assert!(Host::connect(&discovery).is_err());
        assert!(Host::connect_explicit(&discovery).is_ok());
        assert!(!format!("{discovery:?}").contains(&discovery.token));
    }
}
