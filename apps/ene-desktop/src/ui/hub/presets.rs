//! Connection presets and first-run quick setup helpers for [`Hub`].

use super::{Hub, SetupStage};
use crate::rpc::{
    self,
    connections::{SetupConnection, SetupSelection},
};
use ene_client_core::{
    SAKURA_AI_ENDPOINT as SAKURA_ENDPOINT, SearchAdapter,
    domain::LlmProvider,
    presets::{
        EmbeddingPresetSpec, SttPresetSpec, build_embedding_preset_input, build_llm_provider_input,
        build_openai_tts_preset_input, build_sakura_tts_preset_input,
        build_search_assignment_inputs, build_search_preset_input, build_stt_preset_input,
        build_voicevox_preset_input, find_reusable_quick_connect_connection,
    },
};
use gpui::Context;

pub(crate) fn assign_search_connection(
    hub: &mut Hub,
    connection_id: Option<&str>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    let inputs = build_search_assignment_inputs(&settings.connections, connection_id);
    if inputs.is_empty() {
        return;
    }
    hub.update_connections("Web 検索接続の割当", settings.revision, inputs, None, cx);
}

pub(crate) fn add_search_preset(
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

pub(crate) fn add_llm_from_provider(
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

pub(crate) fn save_setup_connection(
    hub: &mut Hub,
    provider: LlmProvider,
    secret: String,
    cx: &mut Context<Hub>,
) -> bool {
    let Some(settings) = hub.model.settings() else {
        return false;
    };
    if hub.setup.stage != SetupStage::Idle {
        return false;
    }
    // Reuse a failed attempt only at the exact catalog endpoint.
    let existing = find_reusable_quick_connect_connection(&settings.connections, &provider);
    let id = existing.map_or_else(|| uuid::Uuid::new_v4().to_string(), |c| c.id.clone());
    let revision = settings.revision;
    let create = existing.is_none();
    let input = build_llm_provider_input(id.clone(), provider, Vec::new());
    if !rpc::connections::save_setup_connection(
        &hub.rpc,
        hub.model.connections.link_epoch,
        SetupConnection {
            revision,
            id,
            create,
            input,
            secret,
        },
    ) {
        return false;
    }
    hub.setup.stage = SetupStage::SavingConnection;
    hub.setup.error = None;
    cx.notify();
    true
}

pub(crate) fn complete_setup_selection(
    hub: &mut Hub,
    connection_id: String,
    model_id: &str,
    companion_name: Option<String>,
    voice_identifier: Option<String>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    if !matches!(hub.setup.stage, SetupStage::Idle | SetupStage::Choosing(_))
        || hub
            .setup
            .connection_id()
            .is_some_and(|id| id != connection_id)
    {
        return;
    }
    let model_id = model_id.trim().to_owned();
    if model_id.is_empty() {
        return;
    }
    if !settings.connections.iter().any(|connection| {
        connection.id == connection_id && hub.setup.can_apply(connection, &model_id)
    }) {
        return;
    }
    let existing_comp = hub.model.companion().cloned();
    let voice_clean = voice_identifier
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let name_clean = companion_name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "ene".to_owned());

    if rpc::connections::complete_setup_selection(
        &hub.rpc,
        hub.model.connections.link_epoch,
        SetupSelection {
            connection_id: connection_id.clone(),
            model_id,
            existing_comp,
            companion_name: name_clean,
            voice_identifier: voice_clean,
        },
    ) {
        hub.setup.stage = SetupStage::Applying(connection_id);
        hub.setup.error = None;
        cx.notify();
    }
}

pub(crate) fn add_voicevox_preset(
    hub: &mut Hub,
    speaker: u32,
    speed: Option<f32>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    if !hub.rpc.has_host() {
        return;
    }
    let existing_rev = settings.connection("voicevox").map_or(0, |c| c.revision);
    let input = build_voicevox_preset_input(existing_rev, speaker, speed);
    hub.update_connections(
        "VOICEVOX接続の追加",
        settings.revision,
        vec![input],
        Some("voicevox".into()),
        cx,
    );
}

pub(crate) fn add_openai_tts_preset(
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

pub(crate) fn add_stt_preset(
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

pub(crate) fn add_sakura_tts_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
    let input = {
        let connections = hub
            .model
            .settings()
            .map_or(&[][..], |s| s.connections.as_slice());
        build_sakura_tts_preset_input(connections)
    };
    hub.upsert_connection("さくらのAI Engine TTS接続の追加", input, cx);
}

pub(crate) fn add_sakura_stt_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
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

pub(crate) fn add_sakura_embedding_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
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

pub(crate) fn add_openai_embedding_preset(hub: &mut Hub, cx: &mut Context<Hub>) {
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
    let input = {
        let connections = hub
            .model
            .settings()
            .map_or(&[][..], |s| s.connections.as_slice());
        build_embedding_preset_input(connections, spec)
    };
    hub.upsert_connection("埋め込み（Embedding）接続の追加", input, cx);
}
