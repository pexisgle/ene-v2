//! Which probe a connection gets, plus the MCP probe that polls for tools.

use super::super::Services;
use super::probe::{failed, healthy, probe_embedding, probe_openai_speech, probe_voicevox_tts};
use crate::host::{
    error::{HostError, Result},
    mcp::CheckState,
    model,
    settings::{Check, Connection, ConnectionConfig},
};
use ene_store::util::now;
use std::time::Duration;

pub(super) async fn run_connection_probe(
    services: &Services,
    credentials: &model::Credentials,
    connection: &Connection,
    created_revision: u64,
    credential_generation: u64,
) -> Result<Check> {
    let providers = services.runtime.providers();
    Ok(match &connection.config {
        ConnectionConfig::Mcp(config) => {
            return probe_mcp_connection(services, connection, &config.command).await;
        }
        ConnectionConfig::Tts(_)
            if connection
                .endpoint()
                .adapter
                .eq_ignore_ascii_case("voicevox")
                && !model::is_sakura_endpoint(&connection.endpoint().endpoint)
                && !providers.claims("voicevox", ene_plugin::Extension::Tts) =>
        {
            probe_voicevox_tts(connection, &services.runtime.voicevox()).await
        }
        ConnectionConfig::Stt(_) | ConnectionConfig::Tts(_) => {
            probe_openai_speech(connection, credentials, &providers).await
        }
        ConnectionConfig::Embedding(config) => {
            probe_embedding(connection, credentials, &providers, config.dimensions).await
        }
        ConnectionConfig::Search(_) => {
            crate::host::web_search::probe(
                &services.runtime.services().store,
                connection,
                created_revision,
                credentials,
                &providers,
                credential_generation,
            )
            .await
        }
        _ => model::probe(credentials, connection, &providers).await,
    })
}

async fn probe_mcp_connection(
    services: &Services,
    connection: &Connection,
    command: &str,
) -> Result<Check> {
    if !connection.enabled {
        return Ok(failed(now(), "MCP 接続が無効化されています。"));
    }
    let reconnect_id = connection.id.clone();
    services
        .runtime
        .change(move |services| {
            services.mcp.request_reconnect(reconnect_id.as_str());
            Ok(())
        })
        .await?;
    // npm may need to download the package before the server can list tools.
    let (timeout, message) = if crate::host::mcp::is_npx_command(command) {
        (
            60,
            "MCP に接続できませんでした。ホストで Node.js と npm が使えること、起動コマンドと通信環境を確認してください。",
        )
    } else {
        (
            20,
            "MCP に接続できませんでした。サーバー URL または起動コマンドを確認してください。",
        )
    };
    tokio::time::timeout(Duration::from_secs(timeout), async {
        loop {
            let id = connection.id.clone();
            let revision = connection.revision;
            let state = services
                .runtime
                .inspect(move |services| {
                    if !services
                        .store
                        .settings_repo()
                        .connection(id.as_str())?
                        .is_some_and(|current| current.revision == revision)
                    {
                        return Err(HostError::Conflict(
                            "MCP connection changed during the check".into(),
                        ));
                    }
                    Ok(services
                        .mcp
                        .check_state(Some(&services.credentials), id.as_str(), revision))
                })
                .await?;
            match state {
                CheckState::Connected(tools) => return Ok(healthy(now(), tools)),
                CheckState::Failed(reason) => {
                    return Ok(failed(now(), format!("{message} ({reason})")));
                }
                CheckState::Connecting => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| Ok(failed(now(), message)))
}
