//! Connection presets and first-run quick setup helpers on [`Hub`].

use super::Hub;
use crate::rpc::{
    self,
    connections::{QuickConnectRequest, SetupSelection},
};
use ene_client_core::{
    ModelRoleSlot, SAKURA_AI_ENDPOINT as SAKURA_ENDPOINT,
    domain::LlmProvider,
    presets::{
        EmbeddingPresetSpec, SearchPresetSpec, SttPresetSpec, build_embedding_preset_input,
        build_llm_provider_input, build_openai_tts_preset_input, build_sakura_tts_preset_input,
        build_search_assignment_inputs, build_search_preset_inputs, build_stt_preset_input,
        build_voicevox_preset_input, find_reusable_quick_connect_connection,
        search_provider_defaults,
    },
};
use gpui::Context;

impl Hub {
    pub(crate) fn assign_search_connection(
        &mut self,
        connection_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let inputs =
            build_search_assignment_inputs(&settings.connections, connection_id.as_deref());
        if inputs.is_empty() {
            return;
        }
        rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            "Web 検索接続の割当",
            settings.revision,
            inputs,
        );
        cx.notify();
    }

    pub(crate) fn add_search_preset(
        &mut self,
        spec: SearchPresetSpec<'_>,
        cx: &mut Context<Self>,
    ) -> String {
        let Some(settings) = self.model.settings().cloned() else {
            return spec.preset_id.to_owned();
        };
        if self.rpc.host().is_none() {
            return spec.preset_id.to_owned();
        }
        let (target_id, inputs) = build_search_preset_inputs(&settings.connections, &spec, true);
        rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            "Web 検索接続の追加",
            settings.revision,
            inputs,
        );
        cx.notify();
        target_id
    }

    pub(crate) fn save_search_provider_key(
        &mut self,
        adapter: &'static str,
        secret: String,
        activate: bool,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        let Some((preset_id, name, endpoint)) = search_provider_defaults(adapter) else {
            return;
        };
        if secret.is_empty() || self.rpc.host().is_none() {
            return;
        }
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let spec = SearchPresetSpec {
            preset_id,
            name,
            adapter,
            endpoint,
            activate,
        };
        let (target_id, inputs) = build_search_preset_inputs(&settings.connections, &spec, false);
        self.checking_connections.insert(target_id.clone());
        rpc::connections::save_search_provider_key(
            &self.rpc,
            self.model.connections.link_epoch,
            settings.revision,
            inputs,
            target_id,
            secret,
        );
        cx.notify();
    }

    pub(crate) fn add_llm_from_provider(
        &mut self,
        provider: LlmProvider,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.upsert_connection(
            "接続の追加",
            build_llm_provider_input(uuid::Uuid::new_v4().to_string(), provider, model_ids),
            cx,
        );
    }

    pub(crate) fn quick_connect_llm(
        &mut self,
        provider: LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        if self.rpc.host().is_none() || self.setup_pending {
            return;
        }
        // Reuse a failed attempt only at the exact catalog endpoint.
        let existing = find_reusable_quick_connect_connection(&settings.connections, &provider);
        let id = existing
            .map(|c| c.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let revision = settings.revision;
        let create = existing.is_none();
        let input = build_llm_provider_input(id.clone(), provider, Vec::new());
        self.setup_pending = true;
        self.setup_error = None;
        self.setup_error_detail = None;
        rpc::connections::quick_connect_llm(
            &self.rpc,
            self.model.connections.link_epoch,
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

    pub(crate) fn complete_setup_selection(
        &mut self,
        connection_id: String,
        model_id: String,
        companion_name: Option<String>,
        voice_identifier: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        if self.rpc.host().is_none() {
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
        let existing_comp = self.model.companion().cloned();
        let voice_clean = voice_identifier
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let name_clean = companion_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "ene".to_owned());

        self.pending_setup_connection_id = None;
        self.pending_setup_models.clear();

        rpc::connections::complete_setup_selection(
            &self.rpc,
            self.model.connections.link_epoch,
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

    pub(crate) fn add_voicevox_preset(
        &mut self,
        speaker: u32,
        speed: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        if self.rpc.host().is_none() {
            return;
        }
        let existing_rev = settings
            .connections
            .iter()
            .find(|c| c.id == "voicevox")
            .map(|c| c.revision)
            .unwrap_or(0);
        let input = build_voicevox_preset_input(existing_rev, speaker, speed);
        self.checking_connections.insert("voicevox".into());
        rpc::connections::update_settings_and_probe(
            &self.rpc,
            self.model.connections.link_epoch,
            "VOICEVOX接続の追加",
            settings.revision,
            vec![input],
            "voicevox".into(),
        );
        cx.notify();
    }

    pub(crate) fn add_openai_tts_preset(
        &mut self,
        endpoint: String,
        model: String,
        voice: String,
        cx: &mut Context<Self>,
    ) {
        self.upsert_connection(
            "TTS接続の追加",
            build_openai_tts_preset_input(&endpoint, &model, &voice),
            cx,
        );
    }

    pub(crate) fn add_stt_preset(
        &mut self,
        name: String,
        endpoint: String,
        model: String,
        language: String,
        cx: &mut Context<Self>,
    ) {
        self.add_stt_preset_with_spec(
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

    fn add_stt_preset_with_spec(&mut self, spec: SttPresetSpec<'_>, cx: &mut Context<Self>) {
        let input = {
            let connections = self
                .model
                .settings()
                .map(|s| s.connections.as_slice())
                .unwrap_or(&[]);
            build_stt_preset_input(connections, spec)
        };
        self.upsert_connection("音声認識（STT）接続の追加", input, cx);
    }

    pub(crate) fn add_sakura_tts_preset(&mut self, cx: &mut Context<Self>) {
        let input = {
            let connections = self
                .model
                .settings()
                .map(|s| s.connections.as_slice())
                .unwrap_or(&[]);
            build_sakura_tts_preset_input(connections)
        };
        self.upsert_connection("さくらのAI Engine TTS接続の追加", input, cx);
    }

    pub(crate) fn add_sakura_stt_preset(&mut self, cx: &mut Context<Self>) {
        self.add_stt_preset_with_spec(
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

    pub(crate) fn add_sakura_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            EmbeddingPresetSpec {
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

    pub(crate) fn add_openai_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            EmbeddingPresetSpec {
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

    fn add_embedding_preset(&mut self, spec: EmbeddingPresetSpec<'_>, cx: &mut Context<Self>) {
        let should_assign = self.model.companion().is_some_and(|c| {
            c.assignments
                .as_ref()
                .and_then(|a| a.embedding.as_ref())
                .is_none_or(|e| e.connection_id.trim().is_empty())
        });
        let target = (spec.id.to_owned(), spec.model_id.to_owned());
        let input = {
            let connections = self
                .model
                .settings()
                .map(|s| s.connections.as_slice())
                .unwrap_or(&[]);
            build_embedding_preset_input(connections, spec)
        };
        self.upsert_connection("埋め込み（Embedding）接続の追加", input, cx);
        if should_assign {
            self.assign_model(ModelRoleSlot::Embedding, Some(target), cx);
        }
    }
}
