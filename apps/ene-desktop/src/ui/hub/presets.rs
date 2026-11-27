//! Connection presets and first-run quick setup helpers for [`Hub`].

use super::Hub;
use crate::rpc::{
    self,
    connections::{QuickConnectRequest, SetupSelection},
};
use ene_client_core::{
    ModelRoleSlot, SAKURA_AI_ENDPOINT as SAKURA_ENDPOINT, SearchAdapter,
    domain::LlmProvider,
    presets::{
        EmbeddingPresetSpec, SttPresetSpec, build_embedding_preset_input, build_llm_provider_input,
        build_openai_tts_preset_input, build_sakura_tts_preset_input,
        build_search_assignment_inputs, build_search_preset_input, build_stt_preset_input,
        build_voicevox_preset_input, find_reusable_quick_connect_connection,
    },
};
use gpui::Context;

pub(super) fn assign_search_connection(
    hub: &mut Hub,
    connection_id: Option<&str>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings().cloned() else {
        return;
    };
    let inputs = build_search_assignment_inputs(&settings.connections, connection_id);
    if inputs.is_empty() {
        return;
    }
    rpc::connections::update_settings(
        &hub.rpc,
        hub.model.connections.link_epoch,
        "Web 検索接続の割当",
        settings.revision,
        inputs,
        None,
    );
    cx.notify();
}

pub(super) fn add_search_preset(
    hub: &mut Hub,
    adapter: SearchAdapter,
    cx: &mut Context<Hub>,
) -> String {
    let Some(settings) = hub.model.settings() else {
        return adapter.provider().preset_id.into();
    };
    let (target, input) = build_search_preset_input(&settings.connections, adapter);
    if let Some(input) = input {
        hub.upsert_connection("Web 検索接続の追加", input, cx);
    }
    target
}

pub(super) fn add_llm_from_provider(
    hub: &mut Hub,
    provider: LlmProvider,
    model_ids: Vec<String>,
    cx: &mut Context<Hub>,
) {
    hub.upsert_connection(
        "接続の追加",
        build_llm_provider_input(uuid::Uuid::new_v4().to_string(), provider, model_ids),
        cx,
    );
}

pub(super) fn quick_connect_llm(
    hub: &mut Hub,
    provider: LlmProvider,
    secret: String,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    if hub.rpc.host().is_none() || hub.setup.pending {
        return;
    }
    // Reuse a failed attempt only at the exact catalog endpoint.
    let existing = find_reusable_quick_connect_connection(&settings.connections, &provider);
    let id = existing.map_or_else(|| uuid::Uuid::new_v4().to_string(), |c| c.id.clone());
    let revision = settings.revision;
    let create = existing.is_none();
    let input = build_llm_provider_input(id.clone(), provider, Vec::new());
    hub.setup.pending = true;
    hub.setup.error = None;
    hub.setup.error_detail = None;
    rpc::connections::quick_connect_llm(
        &hub.rpc,
        hub.model.connections.link_epoch,
        QuickConnectRequest {
            revision,
            id,
            create,
            input,
            secret,
        },
    );
    cx.notify();
}

pub(super) fn complete_setup_selection(
    hub: &mut Hub,
    connection_id: String,
    model_id: &str,
    companion_name: Option<String>,
    voice_identifier: Option<String>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings().cloned() else {
        return;
    };
    if hub.rpc.host().is_none() {
        return;
    }
    let model_id = model_id.trim().to_owned();
    if model_id.is_empty() {
        return;
    }
    let conn = settings
        .connections
        .iter()
        .find(|c| c.id == connection_id)
        .cloned();
    let existing_comp = hub.model.companion().cloned();
    let voice_clean = voice_identifier
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let name_clean = companion_name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "ene".to_owned());

    hub.setup.pending_connection_id = None;
    hub.setup.pending_models.clear();

    rpc::connections::complete_setup_selection(
        &hub.rpc,
        hub.model.connections.link_epoch,
        SetupSelection {
            settings_revision: settings.revision,
            conn,
            connection_id,
            model_id,
            existing_comp,
            companion_name: name_clean,
            voice_identifier: voice_clean,
        },
    );
    cx.notify();
}

pub(super) fn add_voicevox_preset(
    hub: &mut Hub,
    speaker: u32,
    speed: Option<f32>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings().cloned() else {
        return;
    };
    if hub.rpc.host().is_none() {
        return;
    }
    let existing_rev = settings
        .connections
        .iter()
        .find(|c| c.id == "voicevox")
        .map_or(0, |c| c.revision);
    let input = build_voicevox_preset_input(existing_rev, speaker, speed);
    hub.setup.checking_connections.insert("voicevox".into());
    rpc::connections::update_settings(
        &hub.rpc,
        hub.model.connections.link_epoch,
        "VOICEVOX接続の追加",
        settings.revision,
        vec![input],
        Some("voicevox".into()),
    );
    cx.notify();
}

pub(super) fn add_openai_tts_preset(
    hub: &mut Hub,
    endpoint: &str,
    model: &str,
    voice: &str,
    cx: &mut Context<Hub>,
) {
    hub.upsert_connection(
        "TTS接続の追加",
        build_openai_tts_preset_input(endpoint, model, voice),
        cx,
    );
}

pub(super) fn add_stt_preset(
    hub: &mut Hub,
    name: String,
    endpoint: String,
    model: String,
    language: String,
    cx: &mut Context<Hub>,
) {
    add_stt_preset_with_spec(
        hub,
        SttPresetSpec {
            fixed_id: None,
            adapter: "openai",
            name,
            endpoint,
            model,
            language,
        },
        cx,
    );
}

fn add_stt_preset_with_spec(hub: &mut Hub, spec: SttPresetSpec<'_>, cx: &mut Context<Hub>) {
    let input = {
        let connections = hub
            .model
            .settings()
            .map_or(&[][..], |s| s.connections.as_slice());
        build_stt_preset_input(connections, spec)
    };
    hub.upsert_connection("音声認識（STT）接続の追加", input, cx);
}

pub(super) fn add_sakura_tts_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
    let input = {
        let connections = hub
            .model
            .settings()
            .map_or(&[][..], |s| s.connections.as_slice());
        build_sakura_tts_preset_input(connections)
    };
    hub.upsert_connection("さくらのAI Engine TTS接続の追加", input, cx);
}

pub(super) fn add_sakura_stt_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
    add_stt_preset_with_spec(
        hub,
        SttPresetSpec {
            fixed_id: Some("sakura-stt"),
            adapter: "sakura",
            name: "さくらのAI Engine STT".into(),
            endpoint: SAKURA_ENDPOINT.into(),
            model: "whisper-large-v3-turbo".into(),
            language: "ja".into(),
        },
        cx,
    );
}

pub(super) fn add_sakura_embedding_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
    add_embedding_preset(
        hub,
        &EmbeddingPresetSpec {
            id: "sakura-embedding",
            name: "さくらのAI Engine Embedding",
            adapter: "sakura",
            endpoint: SAKURA_ENDPOINT,
            model_id: "multilingual-e5-large",
            dimensions: 1024,
        },
        cx,
    );
}

pub(super) fn add_openai_embedding_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
    add_embedding_preset(
        hub,
        &EmbeddingPresetSpec {
            id: "openai-embedding",
            name: "OpenAI Embedding",
            adapter: "openai",
            endpoint: "https://api.openai.com/v1",
            model_id: "text-embedding-3-small",
            dimensions: 1536,
        },
        cx,
    );
}

fn add_embedding_preset(hub: &mut Hub, spec: &EmbeddingPresetSpec<'_>, cx: &mut Context<Hub>) {
    let should_assign = hub.model.companion().is_some_and(|c| {
        c.assignments
            .as_ref()
            .and_then(|a| a.embedding.as_ref())
            .is_none_or(|e| e.connection_id.trim().is_empty())
    });
    let target = (spec.id.to_owned(), spec.model_id.to_owned());
    let input = {
        let connections = hub
            .model
            .settings()
            .map_or(&[][..], |s| s.connections.as_slice());
        build_embedding_preset_input(connections, spec)
    };
    hub.upsert_connection("埋め込み（Embedding）接続の追加", input, cx);
    if should_assign {
        hub.assign_model(ModelRoleSlot::Embedding, Some(target), cx);
    }
}
