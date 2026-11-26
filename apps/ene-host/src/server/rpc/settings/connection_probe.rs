//! Which probe a connection gets, plus the MCP probe that polls for tools.

use super::super::Services;
use super::probe::probe_embedding;
use super::probe::probe_openai_speech;
use super::probe::probe_voicevox_tts;
use crate::host::db::Store;
use crate::host::error::HostError;
use crate::host::model;
use crate::host::settings;
use crate::host::settings::Connection;
use connectrpc::ConnectError;

pub(super) async fn run_connection_probe(
    services: &Services,
    store: &Store,
    credentials: &model::Credentials,
    connection: &Connection,
) -> std::result::Result<settings::Check, ConnectError> {
    let providers = services.runtime.providers();
    if connection.kind == settings::ConnectionKind::Mcp {
        return probe_mcp_connection(services, connection).await;
    }
    if connection.kind == settings::ConnectionKind::Tts
        && connection
            .endpoint()
            .adapter
            .eq_ignore_ascii_case("voicevox")
        && !model::is_sakura_endpoint(&connection.endpoint().endpoint)
        && providers.get("voicevox").is_none()
    {
        let voicevox = services.runtime.voicevox();
        return Ok(probe_voicevox_tts(connection, &voicevox).await);
    }
    if matches!(
        connection.kind,
        settings::ConnectionKind::Stt | settings::ConnectionKind::Tts
    ) {
        return Ok(probe_openai_speech(connection, credentials, &providers).await);
    }
    if connection.kind == settings::ConnectionKind::Embedding {
        return Ok(probe_embedding(connection, credentials, &providers).await);
    }
    if connection.kind == settings::ConnectionKind::Search {
        return Ok(crate::host::web_search::probe(connection, credentials, &providers).await);
    }
    Ok(model::probe(store, credentials, connection, &providers).await)
}

async fn probe_mcp_connection(
    services: &Services,
    connection: &Connection,
) -> std::result::Result<settings::Check, ConnectError> {
    if !connection.enabled {
        return Ok(settings::Check {
            healthy: false,
            checked_at: ene_host_core::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                "MCP 接続が無効化されています。",
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        });
    }
    let reconnect_id = connection.id.clone();
    services
        .call(move |app| {
            if let Some(mut supervisor) = app
                .world_mut()
                .get_resource_mut::<crate::host::mcp::McpSupervisor>()
            {
                crate::host::mcp::request_reconnect(&mut supervisor, &reconnect_id);
            }
            Ok(())
        })
        .await?;
    let uses_npx = matches!(
        &connection.config,
        settings::ConnectionConfig::Mcp(config) if crate::host::mcp::is_npx_command(&config.command)
    );
    // npm may have to download a package before the server can list tools.
    let timeout = std::time::Duration::from_secs(if uses_npx { 60 } else { 20 });
    let outcome = tokio::time::timeout(timeout, async {
        loop {
            let id = connection.id.clone();
            let revision = connection.revision;
            let state = services
                .runtime
                .read(move |app| {
                    let world = app.world();
                    if !world
                        .resource::<Store>()
                        .settings_repo()
                        .connection(&id)?
                        .is_some_and(|current| current.revision == revision)
                    {
                        return Err(HostError::Conflict(
                            "MCP connection changed during the check".into(),
                        ));
                    }
                    Ok(crate::host::mcp::check_state(world, &id, revision))
                })
                .await?;
            match state {
                crate::host::mcp::CheckState::Connected(tools) => {
                    return Ok::<_, HostError>(Ok(tools));
                }
                crate::host::mcp::CheckState::Failed(reason) => {
                    return Ok::<_, HostError>(Err(reason));
                }
                crate::host::mcp::CheckState::Connecting => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    let default_message = if uses_npx {
        "MCP に接続できませんでした。ホストで Node.js と npm が使えること、起動コマンドと通信環境を確認してください。"
    } else {
        "MCP に接続できませんでした。サーバー URL または起動コマンドを確認してください。"
    };
    match outcome {
        Ok(Ok(Ok(tools))) => Ok(settings::Check {
            healthy: true,
            checked_at: ene_host_core::util::now(),
            failure: None,
            capabilities: Vec::new(),
            resources: tools,
        }),
        Ok(Ok(Err(reason))) => Ok(settings::Check {
            healthy: false,
            checked_at: ene_host_core::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                format!("{default_message} ({reason})"),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        }),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Ok(settings::Check {
            healthy: false,
            checked_at: ene_host_core::util::now(),
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                default_message,
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        }),
    }
}
