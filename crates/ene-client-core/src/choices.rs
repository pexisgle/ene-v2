//! Pure view-model choice derivation and default model auto-assignment logic.

use crate::connections::{
    active_stt_in_connections, active_tts_in_connections, tts_voice_of_connection,
};
use crate::domain::{
    AutoAssignModelPatch, Connection, ConnectionConfig, ConnectionKind, ModelAssignment,
    ModelAssignments, ModelInfo,
};
use crate::state::Model;
use ene_catalog::{
    GOOGLE_AGENT_PLATFORM, OPENAI_TTS_VOICES, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS,
    SAKURA_TTS_VOICES, default_models_for_adapter, parse_voice_resource,
};
use std::collections::{BTreeMap, HashSet};

/// Internal host model role slots, exposed only inside progressive disclosure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRoleSlot {
    Companion,
    Task,
    Perception,
    Compaction,
    Consolidation,
    Decision,
    Embedding,
}

impl ModelRoleSlot {
    pub const ALL: [ModelRoleSlot; 7] = [
        ModelRoleSlot::Companion,
        ModelRoleSlot::Task,
        ModelRoleSlot::Perception,
        ModelRoleSlot::Compaction,
        ModelRoleSlot::Consolidation,
        ModelRoleSlot::Decision,
        ModelRoleSlot::Embedding,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Companion => "会話モデル（主要）",
            Self::Task => "作業モデル（主要）",
            Self::Perception => "目（画面・活動の評価）",
            Self::Compaction => "会話履歴の圧縮",
            Self::Consolidation => "記憶の整理",
            Self::Decision => "権限判定",
            Self::Embedding => "埋め込み検索",
        }
    }

    pub fn fallback_hint(self) -> &'static str {
        match self {
            Self::Companion => "自動（接続先から割当）",
            Self::Task => "自動（接続先から割当）",
            Self::Perception | Self::Compaction | Self::Consolidation => "自動（会話モデルを使用）",
            Self::Decision => "自動（作業モデルを使用）",
            Self::Embedding => "未設定（キーワード検索のみ）",
        }
    }

    pub fn current(self, assignments: &ModelAssignments) -> Option<ModelAssignment> {
        let picked = match self {
            Self::Companion => assignments.companion.as_ref(),
            Self::Task => assignments.task.as_ref(),
            Self::Perception => assignments.perception.as_ref(),
            Self::Compaction => assignments.compaction.as_ref(),
            Self::Consolidation => assignments.consolidation.as_ref(),
            Self::Decision => assignments.decision.as_ref(),
            Self::Embedding => assignments.embedding.as_ref(),
        }?;
        (!picked.connection_id.is_empty() && !picked.model_id.is_empty()).then(|| picked.clone())
    }
}

fn push_voice_unique(voices: &mut Vec<(String, String)>, raw: &str) {
    let (vid, label) = parse_voice_resource(raw);
    if vid.is_empty() {
        return;
    }
    if let Some(existing) = voices.iter_mut().find(|(id, _)| id == &vid) {
        if existing.1 == existing.0 && label != vid {
            existing.1 = label;
        }
    } else {
        voices.push((vid, label));
    }
}

fn push_str_unique(list: &mut Vec<(String, String)>, id: &str) {
    let trimmed = id.trim();
    if !trimmed.is_empty() && !list.iter().any(|(existing, _)| existing == trimmed) {
        list.push((trimmed.to_owned(), trimmed.to_owned()));
    }
}

fn push_unique_model(list: &mut Vec<(String, String)>, id: &str, name: Option<&str>) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    let display = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(id);
    if let Some(existing) = list.iter_mut().find(|(eid, _)| eid == id) {
        if existing.1 == existing.0 && display != id {
            existing.1 = display.to_owned();
        }
    } else {
        list.push((id.to_owned(), display.to_owned()));
    }
}

fn llm_config_parts(connection: &Connection) -> (Vec<String>, &str) {
    match connection.config.as_ref() {
        Some(ConnectionConfig::Llm(llm)) => (
            llm.model_ids.clone(),
            llm.endpoint
                .as_ref()
                .map_or_default(|ep| ep.adapter.as_str()),
        ),
        _ => (Vec::new(), ""),
    }
}

fn verified_google_platform_models(
    connection: &Connection,
    fetched: &[ModelInfo],
) -> Option<Vec<(String, String)>> {
    let check = connection.last_check.as_ref()?;
    if !check
        .capabilities
        .iter()
        .any(|cap| cap == "verified_model_list")
    {
        return None;
    }
    Some(
        check
            .resources
            .iter()
            .map(|id| {
                let label = fetched
                    .iter()
                    .find(|item| &item.id == id)
                    .filter(|item| !item.name.trim().is_empty())
                    .map_or(id, |item| &item.name);
                (id.clone(), label.clone())
            })
            .collect(),
    )
}

impl Model {
    /// Discovered TTS voice choices `(connection_id, voice_id, display_name, connection_name)`
    /// from enabled TTS connections, keeping duplicate `voice_id`s across connections distinct.
    pub fn tts_voice_choices(&self) -> Vec<(String, String, String, String)> {
        let Some(settings) = self.settings() else {
            return Vec::new();
        };
        let mut voices = Vec::new();
        for conn in &settings.connections {
            if conn.kind != ConnectionKind::Tts || !conn.enabled {
                continue;
            }
            for (vid, label) in self.voice_choices_for_connection(&conn.id) {
                voices.push((conn.id.clone(), vid, label, conn.name.clone()));
            }
        }
        voices
    }

    /// Discovered `(voice_id, display_name)` choices for a single TTS connection.
    pub fn voice_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(conn) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let mut voices = Vec::new();
        if let Some(check) = self
            .connections
            .checks
            .get(&conn.id)
            .or(conn.last_check.as_ref())
        {
            for res in &check.resources {
                push_voice_unique(&mut voices, res);
            }
        }
        let sakura = conn.is_sakura();
        if let Some(ConnectionConfig::Tts(tts)) = conn.config.as_ref() {
            for id in &tts.voice_ids {
                push_voice_unique(&mut voices, id);
            }
            if !sakura
                && tts
                    .endpoint
                    .as_ref()
                    .is_some_and(|ep| ep.adapter.trim().eq_ignore_ascii_case("openai"))
            {
                for voice in OPENAI_TTS_VOICES {
                    push_voice_unique(&mut voices, voice);
                }
            }
        }
        if sakura {
            for entry in SAKURA_TTS_VOICES {
                push_voice_unique(&mut voices, entry);
            }
        } else if voices.is_empty()
            && let Some(configured) = tts_voice_of_connection(conn)
        {
            push_voice_unique(&mut voices, &configured);
        }
        voices
    }

    pub fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(conn) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let mut models = Vec::new();
        if let Some(ConnectionConfig::Stt(stt)) = conn.config.as_ref()
            && let Some(ep) = stt.endpoint.as_ref()
            && let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options)
            && let Some(model) = opts.get("model").and_then(|v| v.as_str())
        {
            push_str_unique(&mut models, model);
        }
        let sakura = conn.is_sakura();
        if let Some(check) = self
            .connections
            .checks
            .get(&conn.id)
            .or(conn.last_check.as_ref())
        {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if !sakura || lower.contains("whisper") || lower.contains("transcribe") {
                    push_str_unique(&mut models, res);
                }
            }
        }
        let defaults = if sakura {
            SAKURA_STT_MODELS
        } else {
            &["gpt-4o-mini-transcribe", "gpt-4o-transcribe", "whisper-1"]
        };
        for id in defaults {
            push_str_unique(&mut models, id);
        }
        models
    }

    pub fn embedding_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(conn) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let mut models = Vec::new();
        if let Some(ConnectionConfig::Embedding(emb)) = conn.config.as_ref() {
            push_str_unique(&mut models, &emb.model_id);
        }
        if let Some(check) = self
            .connections
            .checks
            .get(&conn.id)
            .or(conn.last_check.as_ref())
        {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if lower.contains("embed") || lower.contains("e5") {
                    push_str_unique(&mut models, res);
                }
            }
        }
        let defaults = if conn.is_sakura() {
            SAKURA_EMBEDDING_MODELS
        } else {
            &["text-embedding-3-small", "text-embedding-3-large"]
        };
        for id in defaults {
            push_str_unique(&mut models, id);
        }
        models
    }

    /// Available `(model_id, display_label)` choices for a single LLM connection.
    pub fn available_models_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(connection) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let (configured, adapter) = llm_config_parts(connection);
        let fetched = if adapter == GOOGLE_AGENT_PLATFORM && connection.last_check.is_none() {
            Vec::new()
        } else {
            self.connections
                .models
                .get(&connection.id)
                .cloned()
                .unwrap_or_default()
        };
        if adapter == GOOGLE_AGENT_PLATFORM
            && let Some(verified) = verified_google_platform_models(connection, &fetched)
        {
            return verified;
        }
        let checked = if adapter == GOOGLE_AGENT_PLATFORM {
            connection
                .last_check
                .as_ref()
                .map_or_default(|check| check.resources.clone())
        } else {
            self.connections
                .checks
                .get(&connection.id)
                .or(connection.last_check.as_ref())
                .map_or_default(|check| check.resources.clone())
        };
        let mut list = Vec::new();
        for id in &configured {
            let fetched_name = fetched
                .iter()
                .find(|m| m.id == *id)
                .map(|m| m.name.as_str());
            push_unique_model(&mut list, id, fetched_name);
        }
        for model in &fetched {
            push_unique_model(&mut list, &model.id, Some(&model.name));
        }
        for id in &checked {
            push_unique_model(&mut list, id, None);
        }
        if list.is_empty() {
            for id in default_models_for_adapter(adapter) {
                push_unique_model(&mut list, id, None);
            }
        }
        list
    }

    /// Every `(connection_id, model_id, display_label)` available across enabled LLM connections.
    pub fn model_choices(&self) -> Vec<(String, String, String)> {
        let Some(settings) = self.settings() else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for connection in &settings.connections {
            if connection.kind != ConnectionKind::Llm || !connection.enabled {
                continue;
            }
            for (model_id, label) in self.available_models_for_connection(&connection.id) {
                choices.push((
                    connection.id.clone(),
                    model_id,
                    format!("{} / {label}", connection.name),
                ));
            }
        }
        choices
    }

    pub fn collect_connection_choices<F>(
        &self,
        kind: ConnectionKind,
        choices_for: F,
    ) -> Vec<(String, String, String)>
    where
        F: Fn(&Self, &str) -> Vec<(String, String)>,
    {
        let Some(settings) = self.settings() else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for connection in &settings.connections {
            if connection.kind != kind {
                continue;
            }
            for (id, label) in choices_for(self, &connection.id) {
                choices.push((
                    connection.id.clone(),
                    id,
                    format!("{} / {label}", connection.name),
                ));
            }
        }
        choices
    }

    pub fn all_llm_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Llm, Self::available_models_for_connection)
    }

    pub fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(
            ConnectionKind::Embedding,
            Self::embedding_model_choices_for_connection,
        )
    }

    pub fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Stt, Self::stt_model_choices_for_connection)
    }

    pub fn active_stt_selection(&self) -> Option<(String, String)> {
        active_stt_in_connections(&self.settings()?.connections)
    }

    pub fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Tts, Self::voice_choices_for_connection)
    }

    pub fn active_tts_selection(&self) -> Option<(String, String)> {
        active_tts_in_connections(&self.settings()?.connections)
    }

    /// Computes a default companion/task model auto-assignment patch if needed.
    pub fn auto_assign_default_model_request(
        &self,
        auto_assigned_revisions: &BTreeMap<String, u64>,
    ) -> Option<AutoAssignModelPatch> {
        if !self.connections.link.is_live() {
            return None;
        }
        let providers = self.connections.providers.as_deref()?;
        let companion = self.companion()?;
        if auto_assigned_revisions
            .get(&companion.id)
            .is_some_and(|rev| *rev >= companion.revision)
        {
            return None;
        }
        let settings = self.settings()?;
        let enabled_llm_ids: HashSet<&str> = settings
            .connections
            .iter()
            .filter(|c| c.can_start_chat(providers))
            .map(|c| c.id.as_str())
            .collect();
        let (conn_id, model_id, _) = self
            .model_choices()
            .into_iter()
            .find(|(id, _, _)| enabled_llm_ids.contains(id.as_str()))?;
        let assignments = companion.assignments.as_ref();
        let is_valid = |slot: ModelRoleSlot| -> bool {
            assignments
                .and_then(|a| slot.current(a))
                .is_some_and(|assigned| {
                    !assigned.connection_id.is_empty()
                        && !assigned.model_id.is_empty()
                        && settings.connections.iter().any(|c| {
                            c.id == assigned.connection_id
                                && c.enabled
                                && c.kind == ConnectionKind::Llm
                                && !c.needs_key(providers)
                        })
                })
        };
        let assign_companion = !is_valid(ModelRoleSlot::Companion);
        let assign_task = !is_valid(ModelRoleSlot::Task);
        if !assign_companion && !assign_task {
            return None;
        }
        Some(AutoAssignModelPatch {
            companion_id: companion.id.clone(),
            revision: companion.revision,
            assign_companion,
            assign_task,
            assignment: ModelAssignment {
                connection_id: conn_id,
                model_id,
            },
        })
    }
}
