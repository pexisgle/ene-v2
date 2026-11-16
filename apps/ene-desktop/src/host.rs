//! The desktop client's view of one host: discovery, the generated service
//! clients, and the activation generation every request is bound to.
//!
//! The host publishes `connection.json` into its runtime directory. That file
//! is the only way in: an endpoint that must be loopback `/rpc` and an Owner
//! Bearer, both validated by [`ene_rpc::connect`].

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
        let bytes = std::fs::read(path)
            .map_err(|error| Connect::Unavailable(format!("{}: {error}", path.display())))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| Connect::Unavailable(format!("{}: {error}", path.display())))
    }
}

/// Why the client could not reach or bind to the host.
#[derive(Debug, thiserror::Error)]
pub enum Connect {
    #[error("the host is not reachable: {0}")]
    Unavailable(String),
    #[error("the host rejected the request: {0}")]
    Rejected(String),
}

impl From<connectrpc::ConnectError> for Connect {
    fn from(error: connectrpc::ConnectError) -> Self {
        Self::Rejected(error.to_string())
    }
}

/// The generated clients over one transport. Cloning a client is cheap and
/// shares the connection pool, so each task takes its own copy.
#[derive(Clone)]
pub struct Host {
    pub host: w::HostServiceClient<connectrpc::client::HttpClient>,
    pub chat: w::ChatServiceClient<connectrpc::client::HttpClient>,
    pub settings: w::SettingsServiceClient<connectrpc::client::HttpClient>,
    pub voice: w::VoiceServiceClient<connectrpc::client::HttpClient>,
    pub client: w::ClientServiceClient<connectrpc::client::HttpClient>,
}

impl Host {
    /// Connects to a published endpoint. The endpoint and token are validated
    /// by `ene_rpc::connect`; nothing here widens that.
    pub fn connect(discovery: &Discovery) -> Result<Self, Connect> {
        let (transport, config) = ene_rpc::connect(&discovery.endpoint, &discovery.token)
            .map_err(|error| Connect::Unavailable(error.to_string()))?;
        Ok(Self {
            host: w::HostServiceClient::new(transport.clone(), config.clone()),
            chat: w::ChatServiceClient::new(transport.clone(), config.clone()),
            settings: w::SettingsServiceClient::new(transport.clone(), config.clone()),
            voice: w::VoiceServiceClient::new(transport.clone(), config.clone()),
            client: w::ClientServiceClient::new(transport, config),
        })
    }
}
