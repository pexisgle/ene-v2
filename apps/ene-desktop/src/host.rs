//! The desktop client's view of one host: discovery and the generated clients.
//!
//! The host publishes `connection.json` into its runtime directory. That file is
//! the only way in: an endpoint that must be loopback `/rpc` and an Owner
//! Bearer, both validated by [`ene_rpc::connect`]. The host rewrites the file on
//! every start, so it is read again on every attempt rather than once.

use ene_rpc as w;
use serde::Deserialize;
use std::path::Path;

/// What the host publishes for a client to connect with.
#[derive(Clone, Debug, Deserialize)]
pub struct Discovery {
    pub endpoint: String,
    pub token: String,
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

impl Host {
    /// Connects to a published endpoint. The endpoint and token are validated
    /// by `ene_rpc::connect`; nothing here widens that.
    pub fn connect(discovery: &Discovery) -> Result<Self, Connect> {
        let (transport, config) = ene_rpc::connect(&discovery.endpoint, &discovery.token)
            .map_err(|error| Connect::Invalid(error.to_string()))?;
        Ok(Self {
            host: w::HostServiceClient::new(transport.clone(), config.clone()),
            chat: w::ChatServiceClient::new(transport.clone(), config.clone()),
            settings: w::SettingsServiceClient::new(transport.clone(), config.clone()),
            voice: w::VoiceServiceClient::new(transport.clone(), config.clone()),
            work: w::WorkServiceClient::new(transport.clone(), config.clone()),
            client: w::ClientServiceClient::new(transport, config),
        })
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
}
