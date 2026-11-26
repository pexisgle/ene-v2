//! Settings service.

use super::super::Services;
use super::{is_builtin_voicevox, notify_settings};
use crate::host::wire;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use ene_rpc as w;

pub(super) async fn get_settings(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::Settings> {
    services
        .with_store(|store| Ok(wire::settings(&store.settings_repo().settings()?)))
        .await
}

pub(super) async fn update_settings(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::UpdateSettingsRequest>,
) -> ServiceResult<w::Settings> {
    let request = request.to_owned_message();
    let expected_revision = request.expected_revision;
    let mut inputs = Vec::with_capacity(request.connections.len());
    for input in request.connections {
        let expected = input.expected_revision;
        inputs.push((wire::connection_from(input)?, expected));
    }
    // Pre-start enabled plugins referenced in `inputs` so that when
    // `SettingsUpdated` fires on reload inside `services.call`, newly enabled
    // plugin adapters (e.g. openrouter-wasm) are already running.
    for (conn, _) in &inputs {
        if !conn.enabled {
            continue;
        }
        let adapter = conn.endpoint().adapter.clone();
        if services.runtime.providers().get(&adapter).is_none() {
            continue;
        }
        let extension = match conn.kind {
            crate::host::settings::ConnectionKind::Embedding => ene_plugin::Extension::Embedding,
            crate::host::settings::ConnectionKind::Llm => ene_plugin::Extension::Llm,
            crate::host::settings::ConnectionKind::Tts => ene_plugin::Extension::Tts,
            crate::host::settings::ConnectionKind::Stt => ene_plugin::Extension::Stt,
            crate::host::settings::ConnectionKind::Acp => ene_plugin::Extension::Harness,
            crate::host::settings::ConnectionKind::Mcp
            | crate::host::settings::ConnectionKind::Search => continue,
        };
        let _discarded = services
            .runtime
            .providers()
            .start(&adapter, extension)
            .await;
    }
    let view = services
        .change(move |services| {
            let store = &services.store;
            let settings = store
                .settings_repo()
                .update_settings(expected_revision, &inputs)?;
            let view = wire::settings(&settings);
            notify_settings(&services.notifier, view.clone());
            Ok(view)
        })
        .await?;
    // The save lands first. A plugin starts because a stored connection
    // names it. `start_used` logs a plugin that will not start and carries
    // on, the same policy as startup, so the save still succeeds.
    let services = services.runtime.services();
    let store = &services.store;
    services.providers.start_used(store).await;
    if let Ok(current) = store.settings_repo().settings() {
        let providers = &services.providers;
        let active_vv = current
            .connections
            .iter()
            .find(|conn| is_builtin_voicevox(conn, providers));
        if let Some(vv) = active_vv {
            if !cfg!(test) {
                let mgr = services.voicevox.clone();
                let base = vv.endpoint().endpoint.clone();
                let options = vv.endpoint().options.clone();
                tokio::spawn(async move {
                    if let Err(error) = mgr.ensure_running(&base, &options).await {
                        tracing::warn!(%error, "could not auto-start VOICEVOX engine after settings update");
                    }
                });
            }
        } else {
            services.voicevox.stop().await;
        }
    }
    Ok(view)
}
