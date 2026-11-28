//! Open one admitted MCP connection with a fresh credential and a bounded handshake.
use super::{registration::Receiver, settings::Desired};
use crate::host::{db::Store, model::Credentials};
use ene_domain::settings::{CredentialState, McpConnection as McpConfig};
use ene_work::mcp::{ConnectFailure, is_npx_command};
use rig_agent::tool::server::ToolServerHandle;
use rig_rmcp::McpConnection;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) async fn start(
    store: &Store,
    id: &str,
    want: &Desired,
    keys: Option<&Credentials>,
    registry: ToolServerHandle,
    stop: &CancellationToken,
) -> Result<(McpConnection, Receiver), ConnectFailure> {
    let seconds = if is_npx_command(&want.config.command) {
        55
    } else {
        15
    };
    // A failed or abandoned handshake must close the worker even before its
    // result reaches the actor. The slot owns cancellation after adoption.
    let startup = stop.clone().drop_guard();
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        let secret = read_secret(want.credential_state, keys, id).await?;
        let current = want
            .stamp
            .current(store, keys, id)
            .map_err(|_| ConnectFailure::Transient("MCP settings could not be read".into()))?;
        if !current {
            return Err(ConnectFailure::Configuration(
                "the MCP connection changed before startup".into(),
            ));
        }
        let key = keys.and_then(|keys| keys.approval_key.as_ref());
        let receiver = Receiver::new(id, want, secret.as_deref(), key);
        let connection = open(&want.config, secret.as_deref(), registry, stop).await?;
        Ok((connection, receiver))
    })
    .await
    .unwrap_or_else(|_| {
        Err(ConnectFailure::Transient(
            "MCP initialization timed out".into(),
        ))
    });
    if result.is_ok() {
        let _stop = startup.disarm();
    }
    result
}

async fn read_secret(
    state: CredentialState,
    keys: Option<&Credentials>,
    id: &str,
) -> Result<Option<String>, ConnectFailure> {
    if state != CredentialState::Registered {
        return Ok(None);
    }
    let missing =
        || ConnectFailure::Configuration("the registered MCP credential is unavailable".into());
    let keys = keys.cloned().ok_or_else(missing)?;
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || keys.secret(&id))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| ConnectFailure::Transient("MCP credential store could not be read".into()))?
        .map(Some)
        .ok_or_else(missing)
}

pub(super) async fn open(
    config: &McpConfig,
    secret: Option<&str>,
    registry: ToolServerHandle,
    stop: &CancellationToken,
) -> Result<McpConnection, ConnectFailure> {
    let client = crate::host::net::client()
        .map_err(|error| ConnectFailure::Configuration(error.to_string()))?;
    ene_work::mcp::open(config, secret, registry, client, stop).await
}
