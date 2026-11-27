//! Connection CRUD, model/voice selection, probing, credentials, and preset entrypoints on [`Hub`].

use super::{Hub, presets, voice_roles};
use crate::rpc;
use ene_client_core::{
    SearchAdapter,
    connections::build_connection_enabled_input,
    domain::{Connection, ConnectionInput, ConnectionKind, LlmProvider},
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
        self.update_connections(context, settings.revision, vec![input], None, cx);
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

    pub(crate) fn add_mcp_connection(
        &mut self,
        input: ConnectionInput,
        secret: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(settings) = self.model.settings() else {
            return false;
        };
        let id = input.id.clone();
        let Some(ticket) = rpc::connections::add_mcp_and_check(
            &self.rpc,
            self.model.connections.link_epoch,
            settings.revision,
            input,
            secret,
        ) else {
            return false;
        };
        self.setup.checking_connections.insert(id, ticket);
        cx.notify();
        true
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: &Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let input = build_connection_enabled_input(connection, enabled);
        let probe = (enabled && connection.is_local_voicevox()).then(|| connection.id.clone());
        self.update_connections("接続状態の更新", settings.revision, vec![input], probe, cx);
    }

    pub(super) fn update_connections(
        &mut self,
        context: &'static str,
        revision: u64,
        inputs: Vec<ConnectionInput>,
        probe: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let ticket = rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            context,
            revision,
            inputs,
            probe.clone(),
        );
        if let Some((id, ticket)) = probe.zip(ticket) {
            self.setup.checking_connections.insert(id, ticket);
        }
        cx.notify();
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(revision) = self.model.settings().map(|settings| settings.revision) else {
            return;
        };
        self.check_connection_at(connection_id, revision, cx);
    }

    pub(crate) fn check_connection_at(
        &mut self,
        connection_id: String,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        if self.setup.checking_connections.contains_key(&connection_id) {
            return;
        }
        if let Some(ticket) = rpc::connections::probe_connection(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id.clone(),
            revision,
        ) {
            self.setup
                .checking_connections
                .insert(connection_id, ticket);
            cx.notify();
        }
    }

    /// Assigns the active STT connection and model atomically across all STT connections.
    pub(crate) fn assign_stt_model(
        &mut self,
        selection: Option<(&str, &str)>,
        cx: &mut Context<Self>,
    ) {
        voice_roles::assign_stt_model(self, selection, cx);
    }

    /// Assigns the active TTS connection and voice atomically across all TTS connections,
    /// probing local VOICEVOX when newly enabled and syncing companion `voice_identifier`.
    pub(crate) fn assign_tts_voice(
        &mut self,
        selection: Option<(&str, &str)>,
        cx: &mut Context<Self>,
    ) {
        voice_roles::assign_tts_voice(self, selection, cx);
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: &str,
        secret: &str,
        revision: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim();
        if secret.is_empty() || self.rpc.host().is_none() {
            return;
        }
        rpc::connections::register_secret(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id.to_owned(),
            secret.to_owned(),
            revision,
        );
        cx.notify();
    }

    pub(crate) fn register_secret_for_connections(
        &mut self,
        connection_ids: Vec<String>,
        secret: &str,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim();
        if secret.is_empty() || connection_ids.is_empty() {
            return;
        }
        rpc::connections::register_secret_for_connections(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_ids,
            secret.to_owned(),
            revision,
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
        connection_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        presets::assign_search_connection(self, connection_id, cx);
    }

    pub(crate) fn add_search_preset(
        &mut self,
        adapter: SearchAdapter,
        cx: &mut Context<Self>,
    ) -> String {
        presets::add_search_preset(self, adapter, cx)
    }

    pub(crate) fn add_llm_from_provider(
        &mut self,
        provider: LlmProvider,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        presets::add_llm_from_provider(self, provider, model_ids, cx);
    }

    pub(crate) fn save_setup_connection(
        &mut self,
        provider: LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) -> bool {
        presets::save_setup_connection(self, provider, secret, cx)
    }

    pub(crate) fn complete_setup_selection(
        &mut self,
        connection_id: String,
        model_id: &str,
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
        endpoint: &str,
        model: &str,
        voice: &str,
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
