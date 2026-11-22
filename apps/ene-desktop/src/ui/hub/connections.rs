//! Connection CRUD, model/voice selection, probing, credentials, and preset entrypoints on [`Hub`].

use super::{Hub, presets};
use crate::rpc;
use ene_client_core::{
    ModelRoleSlot,
    connections::{
        apply_stt_model_to_config, apply_tts_voice_to_config, build_connection_enabled_input,
        build_connection_model_input, build_embedding_model_input, build_stt_assignment_inputs,
        build_tts_assignment_inputs,
    },
    domain::{Connection, ConnectionInput, ConnectionKind, LlmProvider},
    presets::SearchPresetSpec,
};
use gpui::Context;

impl Hub {
    pub(crate) fn load_providers(&mut self, _cx: &mut Context<Self>) {
        if self.model.connections.providers.is_some() {
            return;
        }
        rpc::connections::load_providers(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn load_models(&mut self, _cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let ids: Vec<String> = settings
            .connections
            .iter()
            .filter(|connection| connection.kind == ConnectionKind::Llm)
            .map(|connection| connection.id.clone())
            .collect();
        if !ids.is_empty() {
            rpc::connections::load_models(&self.rpc, self.model.connections.link_epoch, ids);
        }
    }

    pub(crate) fn upsert_connection(
        &mut self,
        context: &'static str,
        input: ConnectionInput,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            context,
            settings.revision,
            vec![input],
        );
        cx.notify();
    }

    pub(crate) fn delete_connection(&mut self, id: String, revision: u64, cx: &mut Context<Self>) {
        rpc::connections::delete_connection(
            &self.rpc,
            self.model.connections.link_epoch,
            id,
            revision,
        );
        cx.notify();
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let is_voicevox = connection.is_local_voicevox();
        let conn_id = connection.id.clone();
        let input = build_connection_enabled_input(&connection, enabled);
        if !(enabled && is_voicevox) {
            self.upsert_connection("接続状態の更新", input, cx);
            return;
        }
        if self.rpc.host().is_none() {
            return;
        }
        self.setup.checking_connections.insert(conn_id.clone());
        rpc::connections::update_settings_and_probe(
            &self.rpc,
            self.model.connections.link_epoch,
            "接続状態の更新",
            settings.revision,
            vec![input],
            conn_id,
        );
        cx.notify();
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        if self.rpc.host().is_none() || self.setup.checking_connections.contains(&connection_id) {
            return;
        }
        self.setup
            .checking_connections
            .insert(connection_id.clone());
        rpc::connections::probe_connection(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id,
        );
        cx.notify();
    }

    pub(crate) fn select_connection_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_id = model_id.trim().to_owned();
        if model_id.is_empty() {
            return;
        }
        self.upsert_connection(
            "使用モデルの変更",
            build_connection_model_input(&conn, &model_id),
            cx,
        );
        if let Some(comp) = self.model.companion().cloned() {
            let assignments = comp.assignments.as_ref();
            let comp_uses_conn = assignments
                .and_then(|a| a.companion.as_ref())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            let task_uses_conn = assignments
                .and_then(|a| a.task.as_ref())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            if comp_uses_conn {
                self.assign_model(
                    ModelRoleSlot::Companion,
                    Some((connection_id.clone(), model_id.clone())),
                    cx,
                );
            }
            if task_uses_conn {
                self.assign_model(ModelRoleSlot::Task, Some((connection_id, model_id)), cx);
            }
        }
    }

    pub(crate) fn select_voicevox_speaker(
        &mut self,
        connection_id: String,
        speaker_id: String,
        cx: &mut Context<Self>,
    ) {
        self.select_tts_voice(connection_id, speaker_id, cx);
    }

    pub(crate) fn select_tts_voice(
        &mut self,
        connection_id: String,
        voice_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let voice_clean = voice_id.trim().to_owned();
        if voice_clean.is_empty() {
            return;
        }
        let is_sakura = conn.is_sakura();
        let mut input = conn.to_input();
        if let Some(cfg) = input.config.as_mut() {
            apply_tts_voice_to_config(cfg, is_sakura, &voice_clean);
        }
        self.upsert_connection("音声話者の変更", input, cx);
    }

    pub(crate) fn select_stt_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        let mut input = conn.to_input();
        if let Some(cfg) = input.config.as_mut() {
            apply_stt_model_to_config(cfg, &model_clean);
        }
        self.upsert_connection("音声認識モデルの変更", input, cx);
    }

    /// Assigns the active STT connection and model atomically across all STT connections.
    pub(crate) fn assign_stt_model(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let inputs = build_stt_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, mid)| (cid.as_str(), mid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            "音声認識モデルの割当",
            settings.revision,
            inputs,
        );
        cx.notify();
    }

    /// Assigns the active TTS connection and voice atomically across all TTS connections,
    /// probing local VOICEVOX when newly enabled and syncing companion `voice_identifier`.
    pub(crate) fn assign_tts_voice(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        if self.rpc.host().is_none() {
            return;
        }
        let inputs = build_tts_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, vid)| (cid.as_str(), vid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        let target_voice_clean = selection
            .as_ref()
            .map(|(_, vid)| vid.trim().to_owned())
            .filter(|v| !v.is_empty());
        let probe_voicevox_id = selection.as_ref().and_then(|(cid, _)| {
            let conn = settings.connections.iter().find(|c| c.id == *cid)?;
            (conn.is_local_voicevox()
                && (!conn.enabled || self.voice_choices_for_connection(&conn.id).len() <= 1))
                .then(|| conn.id.clone())
        });
        let epoch = self.model.connections.link_epoch;
        if let Some(check_id) = probe_voicevox_id {
            self.setup.checking_connections.insert(check_id.clone());
            rpc::connections::update_settings_and_probe(
                &self.rpc,
                epoch,
                "音声合成（TTS）の割当",
                settings.revision,
                inputs,
                check_id,
            );
        } else {
            rpc::connections::update_settings(
                &self.rpc,
                epoch,
                "音声合成（TTS）の割当",
                settings.revision,
                inputs,
            );
        }
        self.sync_companion_voice(target_voice_clean);
        cx.notify();
    }

    fn sync_companion_voice(&self, voice: Option<String>) {
        let Some(voice_clean) = voice else {
            return;
        };
        let Some(comp) = self.model.companion() else {
            return;
        };
        if comp.voice_identifier.as_deref() == Some(voice_clean.as_str()) {
            return;
        }
        rpc::companions::sync_companion_voice(
            &self.rpc,
            self.model.connections.link_epoch,
            comp.id.clone(),
            comp.revision,
            voice_clean,
        );
    }

    pub(crate) fn select_embedding_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.model.settings().and_then(|s| {
            s.connections
                .iter()
                .find(|c| c.id == connection_id)
                .cloned()
        }) else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        self.upsert_connection(
            "埋め込みモデルの変更",
            build_embedding_model_input(&conn, &model_clean),
            cx,
        );
        if let Some(comp) = self.model.companion()
            && comp
                .assignments
                .as_ref()
                .and_then(|a| a.embedding.as_ref())
                .is_some_and(|a| a.connection_id == connection_id)
        {
            self.assign_model(
                ModelRoleSlot::Embedding,
                Some((connection_id, model_clean)),
                cx,
            );
        }
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: String,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() || self.rpc.host().is_none() {
            return;
        }
        let is_search = self.model.settings().is_some_and(|s| {
            s.connections
                .iter()
                .any(|c| c.id == connection_id && c.kind == ConnectionKind::Search)
        });
        if is_search {
            self.setup
                .checking_connections
                .insert(connection_id.clone());
        }
        rpc::connections::register_secret(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id,
            secret,
            is_search,
        );
        cx.notify();
    }

    pub(crate) fn register_secret_for_connections(
        &mut self,
        connection_ids: Vec<String>,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() || connection_ids.is_empty() {
            return;
        }
        rpc::connections::register_secret_for_connections(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_ids,
            secret,
        );
        cx.notify();
    }

    pub(crate) fn revoke_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        rpc::connections::revoke_secret(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id,
        );
        cx.notify();
    }

    pub(crate) fn assign_search_connection(
        &mut self,
        connection_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        presets::assign_search_connection(self, connection_id, cx);
    }

    pub(crate) fn add_search_preset(
        &mut self,
        spec: SearchPresetSpec<'_>,
        cx: &mut Context<Self>,
    ) -> String {
        presets::add_search_preset(self, spec, cx)
    }

    pub(crate) fn save_search_provider_key(
        &mut self,
        adapter: &'static str,
        secret: String,
        activate: bool,
        cx: &mut Context<Self>,
    ) {
        presets::save_search_provider_key(self, adapter, secret, activate, cx);
    }

    pub(crate) fn add_llm_from_provider(
        &mut self,
        provider: LlmProvider,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        presets::add_llm_from_provider(self, provider, model_ids, cx);
    }

    pub(crate) fn quick_connect_llm(
        &mut self,
        provider: LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        presets::quick_connect_llm(self, provider, secret, cx);
    }

    pub(crate) fn complete_setup_selection(
        &mut self,
        connection_id: String,
        model_id: String,
        companion_name: Option<String>,
        voice_identifier: Option<String>,
        cx: &mut Context<Self>,
    ) {
        presets::complete_setup_selection(
            self,
            connection_id,
            model_id,
            companion_name,
            voice_identifier,
            cx,
        );
    }

    pub(crate) fn add_voicevox_preset(
        &mut self,
        speaker: u32,
        speed: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        presets::add_voicevox_preset(self, speaker, speed, cx);
    }

    pub(crate) fn add_openai_tts_preset(
        &mut self,
        endpoint: String,
        model: String,
        voice: String,
        cx: &mut Context<Self>,
    ) {
        presets::add_openai_tts_preset(self, endpoint, model, voice, cx);
    }

    pub(crate) fn add_stt_preset(
        &mut self,
        name: String,
        endpoint: String,
        model: String,
        language: String,
        cx: &mut Context<Self>,
    ) {
        presets::add_stt_preset(self, name, endpoint, model, language, cx);
    }

    pub(crate) fn add_sakura_tts_preset(&mut self, cx: &mut Context<Self>) {
        presets::add_sakura_tts_preset(self, cx);
    }

    pub(crate) fn add_sakura_stt_preset(&mut self, cx: &mut Context<Self>) {
        presets::add_sakura_stt_preset(self, cx);
    }

    pub(crate) fn add_sakura_embedding_preset(&mut self, cx: &mut Context<Self>) {
        presets::add_sakura_embedding_preset(self, cx);
    }

    pub(crate) fn add_openai_embedding_preset(&mut self, cx: &mut Context<Self>) {
        presets::add_openai_embedding_preset(self, cx);
    }
}
