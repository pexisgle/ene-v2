//! Generated Connect clients with an owner header. Connecting never changes settings.
use crate::proto::{chat::v1 as w, settings::v1 as s};
use connectrpc::client::{ClientConfig, Http2Connection, SharedHttp2Connection};
use serde::Deserialize;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Deserialize)]
struct Discovery {
    endpoint: String,
    token: String,
}

/// Generated clients sharing an authenticated HTTP/2 connection.
#[derive(Clone)]
pub struct Client {
    pub chat: w::ChatServiceClient<SharedHttp2Connection>,
    pub settings: s::SettingsServiceClient<SharedHttp2Connection>,
}

/// Opens a private discovery file and connects with its owner credential.
pub async fn connect(connection_file: &Path) -> Result<Client> {
    let parent = connection_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = connection_file
        .file_name()
        .ok_or("connection file is required")?;
    let file = private_fs::PrivateDir::open(parent)?.open_file(name)?;
    let discovery: Discovery = serde_json::from_reader(std::io::BufReader::new(file))?;
    open(&discovery.endpoint, &discovery.token).await
}

/// Opens an authenticated HTTP/2 connection without altering host configuration.
pub async fn open(url: &str, token: &str) -> Result<Client> {
    let uri: http::Uri = url.parse()?;
    let connection = Http2Connection::connect_plaintext(uri.clone())
        .await?
        .shared(64);
    let config =
        ClientConfig::new(uri).with_default_header("authorization", format!("Bearer {token}"));
    Ok(Client {
        chat: w::ChatServiceClient::new(connection.clone(), config.clone()),
        settings: s::SettingsServiceClient::new(connection, config),
    })
}
