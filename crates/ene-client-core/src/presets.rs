//! Pure connection preset input builders for search, LLM, voice, and embedding providers.

use crate::domain::{
    Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind,
    EmbeddingConnection, LlmConnection, LlmProvider, SearchConnection, SttConnection,
    TtsConnection,
};

/// Parameters for creating or enabling a search provider preset.
#[derive(Debug)]
pub struct SearchPresetSpec<'a> {
    pub preset_id: &'a str,
    pub name: &'a str,
    pub adapter: &'a str,
    pub endpoint: &'a str,
    pub activate: bool,
}

/// Parameters for creating or updating an STT preset connection.
#[derive(Debug)]
pub struct SttPresetSpec<'a> {
    pub fixed_id: Option<&'a str>,
    pub adapter: &'a str,
    pub name: String,
    pub endpoint: String,
    pub model: String,
    pub language: String,
}

/// Parameters for creating or updating an embedding preset connection.
#[derive(Debug)]
pub struct EmbeddingPresetSpec<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub adapter: &'a str,
    pub endpoint: &'a str,
    pub model_id: &'a str,
    pub dimensions: u32,
}

/// Returns `(preset_id, display_name, default_endpoint)` for a known search provider adapter.
pub fn search_provider_defaults(
    adapter: &str,
) -> Option<(&'static str, &'static str, &'static str)> {
    match adapter {
        "exa" => Some(("search-exa", "Web検索 (Exa)", "https://api.exa.ai/search")),
        "tavily" => Some((
            "search-tavily",
            "Web検索 (Tavily)",
            "https://api.tavily.com/search",
        )),
        "brave" => Some((
            "search-brave",
            "Web検索 (Brave)",
            "https://api.search.brave.com/res/v1/web/search",
        )),
        _ => None,
    }
}

fn search_connection_input(
    id: String,
    name: String,
    adapter: String,
    endpoint: String,
    enabled: bool,
) -> ConnectionInput {
    ConnectionInput {
        id,
        name,
        enabled,
        expected_revision: 0,
        kind: ConnectionKind::Search,
        config: Some(ConnectionConfig::Search(SearchConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter,
                endpoint,
                options_schema: String::new(),
                options: "{}".into(),
            }),
            max_results: 5,
        })),
    }
}

/// Computes `(target_connection_id, inputs)` for adding or activating a search preset.
pub fn build_search_preset_inputs(
    connections: &[Connection],
    spec: &SearchPresetSpec<'_>,
    update_existing_always: bool,
) -> (String, Vec<ConnectionInput>) {
    let existing = connections.iter().find(|conn| {
        conn.id == spec.preset_id
            || conn
                .search_adapter()
                .is_some_and(|existing| existing.eq_ignore_ascii_case(spec.adapter))
    });
    let target_id = existing
        .map(|conn| conn.id.clone())
        .unwrap_or_else(|| spec.preset_id.to_owned());
    let has_enabled_other = connections
        .iter()
        .any(|conn| conn.kind == ConnectionKind::Search && conn.enabled && conn.id != target_id);
    let should_enable = spec.activate || !has_enabled_other || existing.is_some_and(|c| c.enabled);
    let mut inputs = Vec::new();
    if let Some(conn) = existing {
        if update_existing_always || conn.enabled != should_enable {
            let mut input = conn.to_input();
            input.enabled = should_enable;
            inputs.push(input);
        }
    } else {
        inputs.push(search_connection_input(
            target_id.clone(),
            spec.name.to_owned(),
            spec.adapter.to_owned(),
            spec.endpoint.to_owned(),
            should_enable,
        ));
    }
    if spec.activate {
        for conn in connections {
            if conn.kind == ConnectionKind::Search && conn.enabled && conn.id != target_id {
                let mut input = conn.to_input();
                input.enabled = false;
                inputs.push(input);
            }
        }
    }
    (target_id, inputs)
}

/// Builds settings inputs enabling at most one search connection.
pub fn build_search_assignment_inputs(
    connections: &[Connection],
    connection_id: Option<&str>,
) -> Vec<ConnectionInput> {
    connections
        .iter()
        .filter(|conn| conn.kind == ConnectionKind::Search)
        .map(|conn| {
            let mut input = conn.to_input();
            input.enabled = connection_id == Some(conn.id.as_str());
            input
        })
        .collect()
}

/// Builds a new LLM connection input from a catalog provider entry.
pub fn build_llm_provider_input(
    id: String,
    provider: LlmProvider,
    model_ids: Vec<String>,
) -> ConnectionInput {
    ConnectionInput {
        id,
        name: provider.name,
        enabled: true,
        expected_revision: 0,
        kind: ConnectionKind::Llm,
        config: Some(ConnectionConfig::Llm(LlmConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: provider.adapter,
                endpoint: provider.endpoint,
                options_schema: String::new(),
                options: "{}".into(),
            }),
            model_ids,
        })),
    }
}

/// Finds an existing enabled LLM connection matching `provider` eligible for quick-connect reuse.
pub fn find_reusable_quick_connect_connection<'a>(
    connections: &'a [Connection],
    provider: &LlmProvider,
) -> Option<&'a Connection> {
    connections.iter().find(|c| {
        c.enabled
            && c.llm_endpoint().is_some_and(|ep| {
                ep.adapter == provider.adapter
                    && ep.endpoint == provider.endpoint
                    && (ep.adapter != ene_catalog::GOOGLE_AGENT_PLATFORM
                        || serde_json::from_str::<serde_json::Value>(&ep.options)
                            .ok()
                            .and_then(|options| {
                                options["project_id"]
                                    .as_str()
                                    .map(|id| !id.trim().is_empty())
                            })
                            != Some(true))
            })
    })
}

/// Builds the preset input for a local VOICEVOX TTS connection.
pub fn build_voicevox_preset_input(
    existing_rev: u64,
    speaker: u32,
    speed: Option<f32>,
) -> ConnectionInput {
    let options = match speed {
        Some(speed) => format!("{{\"speaker\":{speaker},\"speed\":{speed:.2}}}"),
        None => format!("{{\"speaker\":{speaker}}}"),
    };
    ConnectionInput {
        id: "voicevox".into(),
        name: "VOICEVOX（ローカル）".into(),
        enabled: true,
        expected_revision: existing_rev,
        kind: ConnectionKind::Tts,
        config: Some(ConnectionConfig::Tts(TtsConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: "voicevox".into(),
                endpoint: "http://127.0.0.1:50021".into(),
                options_schema: String::new(),
                options,
            }),
            voice_ids: vec![speaker.to_string()],
        })),
    }
}

/// Builds a new OpenAI-compatible TTS preset connection input.
pub fn build_openai_tts_preset_input(endpoint: &str, model: &str, voice: &str) -> ConnectionInput {
    let model = if model.trim().is_empty() {
        "tts-1"
    } else {
        model.trim()
    };
    let voice = if voice.trim().is_empty() {
        "alloy"
    } else {
        voice.trim()
    };
    let options = serde_json::json!({
        "protocol": "openai",
        "model": model,
        "voice": voice,
    })
    .to_string();
    ConnectionInput {
        id: uuid::Uuid::new_v4().to_string(),
        name: "OpenAI互換 TTS".into(),
        enabled: true,
        expected_revision: 0,
        kind: ConnectionKind::Tts,
        config: Some(ConnectionConfig::Tts(TtsConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: "openai".into(),
                endpoint: if endpoint.trim().is_empty() {
                    "https://api.openai.com/v1".into()
                } else {
                    endpoint.trim().to_owned()
                },
                options_schema: String::new(),
                options,
            }),
            voice_ids: vec![voice.to_owned()],
        })),
    }
}

/// Builds a preset input for an STT connection (OpenAI-compatible or Sakura AI Engine).
pub fn build_stt_preset_input(
    connections: &[Connection],
    spec: SttPresetSpec<'_>,
) -> ConnectionInput {
    let existing_rev = spec
        .fixed_id
        .and_then(|fid| connections.iter().find(|c| c.id == fid).map(|c| c.revision))
        .unwrap_or(0);
    let protocol = if spec.adapter == "sakura" {
        "sakura"
    } else {
        "openai"
    };
    let model = if spec.model.trim().is_empty() {
        "whisper-1"
    } else {
        spec.model.trim()
    };
    let lang = if spec.language.trim().is_empty() {
        "ja".to_owned()
    } else {
        spec.language.trim().to_owned()
    };
    let options = serde_json::json!({
        "protocol": protocol,
        "model": model,
        "language": lang,
    })
    .to_string();
    ConnectionInput {
        id: spec
            .fixed_id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: spec.name,
        enabled: true,
        expected_revision: existing_rev,
        kind: ConnectionKind::Stt,
        config: Some(ConnectionConfig::Stt(SttConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: spec.adapter.to_owned(),
                endpoint: spec.endpoint,
                options_schema: String::new(),
                options,
            }),
            language_hints: vec![lang],
        })),
    }
}

/// Builds the preset input for Sakura AI Engine TTS.
pub fn build_sakura_tts_preset_input(connections: &[Connection]) -> ConnectionInput {
    let existing_rev = connections
        .iter()
        .find(|c| c.id == "sakura-tts")
        .map(|c| c.revision)
        .unwrap_or(0);
    let options = serde_json::json!({
        "protocol": "sakura",
        "model": "zundamon",
        "voice": "normal",
        "response_format": "wav",
    })
    .to_string();
    ConnectionInput {
        id: "sakura-tts".into(),
        name: "さくらのAI Engine TTS".into(),
        enabled: true,
        expected_revision: existing_rev,
        kind: ConnectionKind::Tts,
        config: Some(ConnectionConfig::Tts(TtsConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: "sakura".into(),
                endpoint: ene_catalog::SAKURA_AI_ENDPOINT.into(),
                options_schema: String::new(),
                options,
            }),
            voice_ids: vec!["zundamon/normal".into()],
        })),
    }
}

/// Builds the preset input for an embedding provider connection.
pub fn build_embedding_preset_input(
    connections: &[Connection],
    spec: EmbeddingPresetSpec<'_>,
) -> ConnectionInput {
    let existing_rev = connections
        .iter()
        .find(|c| c.id == spec.id)
        .map(|c| c.revision)
        .unwrap_or(0);
    ConnectionInput {
        id: spec.id.into(),
        name: spec.name.into(),
        enabled: true,
        expected_revision: existing_rev,
        kind: ConnectionKind::Embedding,
        config: Some(ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: spec.adapter.into(),
                endpoint: spec.endpoint.into(),
                options_schema: String::new(),
                options: "{}".into(),
            }),
            model_id: spec.model_id.into(),
            dimensions: spec.dimensions,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_preset_inputs_activate_target_and_disable_other_enabled_search() {
        let (_, init_inputs) = build_search_preset_inputs(
            &[],
            &SearchPresetSpec {
                preset_id: "search-exa",
                name: "Web検索 (Exa)",
                adapter: "exa",
                endpoint: "https://api.exa.ai/search",
                activate: true,
            },
            false,
        );
        assert_eq!(init_inputs.len(), 1);
        assert_eq!(init_inputs[0].id, "search-exa");
        assert!(init_inputs[0].enabled);

        let existing = vec![
            Connection {
                id: "search-exa".into(),
                name: "Exa".into(),
                enabled: true,
                revision: 2,
                kind: ConnectionKind::Search,
                config: init_inputs[0].config.clone(),
                ..Default::default()
            },
            Connection {
                id: "search-tavily".into(),
                name: "Tavily".into(),
                enabled: false,
                revision: 3,
                kind: ConnectionKind::Search,
                config: Some(ConnectionConfig::Search(SearchConnection {
                    endpoint: Some(ConnectionEndpoint {
                        adapter: "tavily".into(),
                        endpoint: "https://api.tavily.com/search".into(),
                        ..Default::default()
                    }),
                    max_results: 5,
                })),
                ..Default::default()
            },
        ];

        let (target_id, switch_inputs) = build_search_preset_inputs(
            &existing,
            &SearchPresetSpec {
                preset_id: "search-tavily",
                name: "Web検索 (Tavily)",
                adapter: "tavily",
                endpoint: "https://api.tavily.com/search",
                activate: true,
            },
            false,
        );
        assert_eq!(target_id, "search-tavily");
        assert_eq!(switch_inputs.len(), 2);
        assert!(
            switch_inputs
                .iter()
                .any(|c| c.id == "search-tavily" && c.enabled)
        );
        assert!(
            switch_inputs
                .iter()
                .any(|c| c.id == "search-exa" && !c.enabled)
        );
    }

    #[test]
    fn sakura_and_voicevox_presets_preserve_expected_defaults() {
        let vv = build_voicevox_preset_input(4, 3, Some(1.1));
        assert_eq!(vv.id, "voicevox");
        assert_eq!(vv.expected_revision, 4);
        assert_eq!(vv.kind, ConnectionKind::Tts);

        let sakura_tts = build_sakura_tts_preset_input(&[]);
        assert_eq!(sakura_tts.id, "sakura-tts");
        assert_eq!(sakura_tts.kind, ConnectionKind::Tts);
    }
}
