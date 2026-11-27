//! Model and voice choices derived from the saved connections and discovered resources.

use crate::connections::{
    active_stt_in_connections, active_tts_in_connections, tts_voice_of_connection,
};
use crate::domain::{Connection, ConnectionConfig, ConnectionKind, ModelInfo};
use crate::state::Model;
use ene_catalog::{
    GOOGLE_AGENT_PLATFORM, GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST, OPENAI_TTS_VOICES,
    SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES, default_models_for_adapter,
    parse_voice_resource,
};
use indexmap::IndexMap;

fn push_voice_unique(voices: &mut IndexMap<String, String>, raw: &str) {
    let (id, label) = parse_voice_resource(raw);
    insert_choice(voices, id, Some(label));
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
        .any(|cap| cap == GOOGLE_AGENT_PLATFORM_VERIFIED_MODEL_LIST)
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
        let mut voices = Vec::new();
        for conn in self
            .connections_of(ConnectionKind::Tts)
            .filter(|conn| conn.enabled)
        {
            for (vid, label) in voice_choices(conn) {
                voices.push((conn.id.clone(), vid, label, conn.name.clone()));
            }
        }
        voices
    }

    /// Discovered `(voice_id, display_name)` choices for a single TTS connection.
    pub fn voice_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.connection_choices(conn_id, voice_choices)
    }

    /// Model choices for one speech-to-text connection.
    pub fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.connection_choices(conn_id, stt_choices)
    }

    /// Model choices for one embedding connection.
    pub fn embedding_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.connection_choices(conn_id, embedding_choices)
    }

    /// Available `(model_id, display_label)` choices for a single LLM connection.
    pub fn available_models_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.connection_choices(conn_id, |conn| self.llm_choices(conn))
    }

    fn llm_choices(&self, connection: &Connection) -> Vec<(String, String)> {
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

    fn connection_choices(
        &self,
        id: &str,
        choices_for: impl FnOnce(&Connection) -> Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        self.settings()
            .and_then(|settings| settings.connections.iter().find(|conn| conn.id == id))
            .map_or_else(Vec::new, choices_for)
    }

    fn connections_of(&self, kind: ConnectionKind) -> impl Iterator<Item = &Connection> {
        self.settings()
            .into_iter()
            .flat_map(|settings| &settings.connections)
            .filter(move |connection| connection.kind == kind)
    }

    fn collect_connection_choices(
        &self,
        kind: ConnectionKind,
        choices_for: impl Fn(&Connection) -> Vec<(String, String)>,
    ) -> Vec<(String, String, String)> {
        let mut choices = Vec::new();
        for connection in self.connections_of(kind) {
            for (id, label) in choices_for(connection) {
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
        self.collect_connection_choices(ConnectionKind::Llm, |conn| self.llm_choices(conn))
    }

    /// Every embedding model offered by every embedding connection.
    pub fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Embedding, embedding_choices)
    }

    /// Every speech-to-text model offered by every such connection.
    pub fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Stt, stt_choices)
    }

    /// The speech-to-text connection and model currently in force.
    pub fn active_stt_selection(&self) -> Option<(String, String)> {
        active_stt_in_connections(&self.settings()?.connections)
    }

    /// Every voice offered by every text-to-speech connection.
    pub fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(ConnectionKind::Tts, voice_choices)
    }

    /// The text-to-speech connection and voice currently in force.
    pub fn active_tts_selection(&self) -> Option<(String, String)> {
        active_tts_in_connections(&self.settings()?.connections)
    }
}

fn voice_choices(conn: &Connection) -> Vec<(String, String)> {
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

fn stt_choices(conn: &Connection) -> Vec<(String, String)> {
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

fn embedding_choices(conn: &Connection) -> Vec<(String, String)> {
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
