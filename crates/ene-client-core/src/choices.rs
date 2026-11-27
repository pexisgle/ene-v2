//! Model and voice choices derived from the saved connections and discovered resources.

use crate::connections::{
    active_stt_in_connections, active_tts_in_connections, tts_voice_of_connection,
};
use crate::domain::{
    Connection, ConnectionConfig, ConnectionKind, ModelAssignment, ModelAssignments, ModelInfo,
};
use crate::state::Model;
use ene_catalog::{
    GOOGLE_AGENT_PLATFORM, OPENAI_TTS_VOICES, SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS,
    SAKURA_TTS_VOICES, default_models_for_adapter, parse_voice_resource,
};
use indexmap::IndexMap;

/// Internal host model role slots, exposed only inside progressive disclosure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRoleSlot {
    /// The model that speaks as the companion.
    Companion,
    /// The model that runs commissioned work.
    Task,
    /// The model that reads screens and other perception input.
    Perception,
    /// The model that condenses long history.
    Compaction,
    /// The model that folds history into memory.
    Consolidation,
    /// The model that rules on policy decisions.
    Decision,
    /// The model that produces embeddings.
    Embedding,
}

impl ModelRoleSlot {
    /// Every role slot, in the order the editor lists them.
    pub const ALL: [ModelRoleSlot; 7] = [
        ModelRoleSlot::Companion,
        ModelRoleSlot::Task,
        ModelRoleSlot::Perception,
        ModelRoleSlot::Compaction,
        ModelRoleSlot::Consolidation,
        ModelRoleSlot::Decision,
        ModelRoleSlot::Embedding,
    ];

    /// Japanese display label for this role slot.
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

    /// What the slot falls back to when the operator sets nothing.
    pub fn fallback_hint(self) -> &'static str {
        match self {
            Self::Companion => "自動（接続先から割当）",
            Self::Task => "自動（接続先から割当）",
            Self::Perception | Self::Compaction | Self::Consolidation => "自動（会話モデルを使用）",
            Self::Decision => "自動（作業モデルを使用）",
            Self::Embedding => "未設定（キーワード検索のみ）",
        }
    }

    /// The assignment in force for this slot, when one is set.
    pub fn current(self, assignments: &ModelAssignments) -> Option<ModelAssignment> {
        let picked = match self {
            Self::Companion => Some(&assignments.companion),
            Self::Task => Some(&assignments.task),
            Self::Perception => assignments.perception.as_ref(),
            Self::Compaction => assignments.compaction.as_ref(),
            Self::Consolidation => assignments.consolidation.as_ref(),
            Self::Decision => assignments.decision.as_ref(),
            Self::Embedding => assignments.embedding.as_ref(),
        }?;
        picked.is_set().then(|| picked.clone())
    }
}

fn push_voice_unique(voices: &mut IndexMap<String, String>, raw: &str) {
    let (id, label) = parse_voice_resource(raw);
    insert_choice(voices, &id, Some(&label));
}

fn insert_choice(choices: &mut IndexMap<String, String>, id: &str, label: Option<&str>) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    let label = label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(id);
    choices
        .entry(id.to_owned())
        .and_modify(|old| {
            if old == id && label != id {
                *old = label.to_owned();
            }
        })
        .or_insert_with(|| label.to_owned());
}

fn llm_config_parts(connection: &Connection) -> (&[String], &str) {
    match connection.config.as_ref() {
        Some(ConnectionConfig::Llm(llm)) => (
            &llm.model_ids,
            llm.endpoint
                .as_ref()
                .map_or_default(|ep| ep.adapter.as_str()),
        ),
        _ => (&[], ""),
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
        let mut voices = IndexMap::new();
        if let Some(check) = &conn.last_check {
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
        voices.into_iter().collect()
    }

    /// Model choices for one speech-to-text connection.
    pub fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(conn) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let mut models = IndexMap::new();
        if let Some(ConnectionConfig::Stt(stt)) = conn.config.as_ref()
            && let Some(ep) = stt.endpoint.as_ref()
            && let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options)
            && let Some(model) = opts.get("model").and_then(|v| v.as_str())
        {
            insert_choice(&mut models, model, None);
        }
        let sakura = conn.is_sakura();
        if let Some(check) = &conn.last_check {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if !sakura || lower.contains("whisper") || lower.contains("transcribe") {
                    insert_choice(&mut models, res, None);
                }
            }
        }
        let defaults = if sakura {
            SAKURA_STT_MODELS
        } else {
            &["gpt-4o-mini-transcribe", "gpt-4o-transcribe", "whisper-1"]
        };
        for id in defaults {
            insert_choice(&mut models, id, None);
        }
        models.into_iter().collect()
    }

    /// Model choices for one embedding connection.
    pub fn embedding_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(conn) = self
            .settings()
            .and_then(|s| s.connections.iter().find(|c| c.id == conn_id))
        else {
            return Vec::new();
        };
        let mut models = IndexMap::new();
        if let Some(ConnectionConfig::Embedding(emb)) = conn.config.as_ref() {
            insert_choice(&mut models, &emb.model_id, None);
        }
        if let Some(check) = &conn.last_check {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if lower.contains("embed") || lower.contains("e5") {
                    insert_choice(&mut models, res, None);
                }
            }
        }
        let defaults = if conn.is_sakura() {
            SAKURA_EMBEDDING_MODELS
        } else {
            &["text-embedding-3-small", "text-embedding-3-large"]
        };
        for id in defaults {
            insert_choice(&mut models, id, None);
        }
        models.into_iter().collect()
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
        let fetched: &[ModelInfo] =
            if adapter == GOOGLE_AGENT_PLATFORM && connection.last_check.is_none() {
                &[]
            } else {
                self.connections
                    .models
                    .get(&connection.id)
                    .map_or(&[], Vec::as_slice)
            };
        if adapter == GOOGLE_AGENT_PLATFORM
            && let Some(verified) = verified_google_platform_models(connection, fetched)
        {
            return verified;
        }
        let checked = connection
            .last_check
            .as_ref()
            .map_or(&[][..], |check| check.resources.as_slice());
        let mut list = IndexMap::new();
        for id in configured {
            insert_choice(&mut list, id, None);
        }
        for model in fetched {
            insert_choice(&mut list, &model.id, Some(&model.name));
        }
        for id in checked {
            insert_choice(&mut list, id, None);
        }
        if list.is_empty() {
            for id in default_models_for_adapter(adapter) {
                insert_choice(&mut list, id, None);
            }
        }
        list.into_iter().collect()
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

    /// Gathers `(connection, value, label)` choices across every connection of one kind.
    pub fn collect_connection_choices<F: Fn(&Self, &str) -> Vec<(String, String)>>(
        &self,
        kind: ConnectionKind,
        choices_for: F,
    ) -> Vec<(String, String, String)> {
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

    /// Every model offered by every LLM connection, with its connection.
    pub fn all_llm_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Llm, Self::available_models_for_connection)
    }

    /// Every embedding model offered by every embedding connection.
    pub fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(
            ConnectionKind::Embedding,
            Self::embedding_model_choices_for_connection,
        )
    }

    /// Every speech-to-text model offered by every such connection.
    pub fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Stt, Self::stt_model_choices_for_connection)
    }

    /// The speech-to-text connection and model currently in force.
    pub fn active_stt_selection(&self) -> Option<(String, String)> {
        active_stt_in_connections(&self.settings()?.connections)
    }

    /// Every voice offered by every text-to-speech connection.
    pub fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Tts, Self::voice_choices_for_connection)
    }

    /// The text-to-speech connection and voice currently in force.
    pub fn active_tts_selection(&self) -> Option<(String, String)> {
        active_tts_in_connections(&self.settings()?.connections)
    }
}
