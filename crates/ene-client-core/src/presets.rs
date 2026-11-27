//! Pure connection preset input builders for search, LLM, voice, and embedding providers.

mod featured;

pub use featured::FeaturedPreset;

use crate::domain::{
    Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind,
    EmbeddingConnection, LlmConnection, LlmProvider, SearchConnection, SttConnection,
    TtsConnection,
};

/// Parameters for creating or updating an STT preset connection.
#[derive(Debug)]
pub struct SttPresetSpec<'a> {
    /// Id to store the connection under, when it must be stable.
    pub fixed_id: Option<&'a str>,
    /// Adapter name the driver selects on.
    pub adapter: &'a str,
    /// Display name shown to the operator.
    pub name: String,
    /// Base URL of the provider.
    pub endpoint: String,
    /// Model to call for this preset.
    pub model: String,
    /// Language hint passed with each request.
    pub language: String,
}

/// Parameters for creating or updating an embedding preset connection.
#[derive(Debug)]
pub struct EmbeddingPresetSpec<'a> {
    /// Id the connection is stored under.
    pub id: &'a str,
    /// Display name shown to the operator.
    pub name: &'a str,
    /// Adapter name the driver selects on.
    pub adapter: &'a str,
    /// Base URL of the provider.
    pub endpoint: &'a str,
    /// Embedding model to call.
    pub model_id: &'a str,
    /// Width of the returned vector.
    pub dimensions: u32,
}

/// Opens the catalog preset's own connection or creates it disabled.
pub fn build_search_preset_input(
    connections: &[Connection],
    adapter: ene_catalog::SearchAdapter,
) -> (String, Option<ConnectionInput>) {
    let provider = adapter.provider();
    let endpoint = provider
        .default_endpoint
        .unwrap_or("http://127.0.0.1:8080/search");
    let reserved = connections
        .iter()
        .find(|conn| conn.id == provider.preset_id);
    if let Some(conn) = reserved
        && let Some(ConnectionConfig::Search(search)) = conn.config.as_ref()
        && let Some(config) = search.endpoint.as_ref()
        && ene_catalog::SearchAdapter::parse(&config.adapter) == Some(adapter)
        && (config.endpoint == endpoint
            || (config.endpoint.trim().is_empty() && provider.default_endpoint.is_some()))
    {
        return (conn.id.clone(), None);
    }
    let id = reserved.map_or_else(
        || provider.preset_id.to_owned(),
        |_| uuid::Uuid::new_v4().to_string(),
    );
    let input = ConnectionInput {
        id: id.clone(),
        name: provider.connection_name.into(),
        enabled: false,
        expected_revision: 0,
        config: Some(ConnectionConfig::Search(SearchConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: provider.adapter_id.into(),
                endpoint: endpoint.into(),
                options_schema: String::new(),
                options: "{}".into(),
            }),
            max_results: 5,
        })),
    };
    (id, Some(input))
}

/// Builds one atomic settings update for an explicit search selection.
pub fn build_search_assignment_inputs(
    connections: &[Connection],
    connection_id: Option<&str>,
) -> Vec<ConnectionInput> {
    let search = |conn: &&Connection| conn.kind == ConnectionKind::Search;
    if connection_id.is_some_and(|id| !connections.iter().filter(search).any(|conn| conn.id == id))
    {
        return Vec::new();
    }
    connections
        .iter()
        .filter(search)
        .filter_map(|conn| {
            let selected = connection_id == Some(conn.id.as_str());
            (conn.enabled != selected).then(|| {
                let mut input = conn.to_input();
                input.enabled = selected;
                input
            })
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
        config: Some(ConnectionConfig::Tts(TtsConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: "voicevox".into(),
                endpoint: crate::VOICEVOX_DEFAULT_ENDPOINT.into(),
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
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned),
        name: spec.name,
        enabled: true,
        expected_revision: existing_rev,
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
        .map_or(0, |c| c.revision);
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
    spec: &EmbeddingPresetSpec<'_>,
) -> ConnectionInput {
    let existing_rev = connections
        .iter()
        .find(|c| c.id == spec.id)
        .map_or(0, |c| c.revision);
    ConnectionInput {
        id: spec.id.into(),
        name: spec.name.into(),
        enabled: true,
        expected_revision: existing_rev,
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
    fn a_search_preset_never_targets_a_custom_endpoint_or_changes_the_active_connection() {
        use ene_catalog::SearchAdapter;

        let (id, preset) = build_search_preset_input(&[], SearchAdapter::Exa);
        let preset = preset.unwrap();
        assert_eq!(id, "search-exa");
        assert!(!preset.enabled);
        let default = Connection {
            id: id.clone(),
            kind: ConnectionKind::Search,
            config: preset.config.clone(),
            ..Default::default()
        };
        assert_eq!(
            build_search_preset_input(std::slice::from_ref(&default), SearchAdapter::Exa),
            (id.clone(), None)
        );

        let mut custom = default;
        custom.enabled = true;
        let Some(ConnectionConfig::Search(config)) = &mut custom.config else {
            panic!("the preset provides a Search config");
        };
        config.endpoint.as_mut().unwrap().endpoint = "https://custom.example/".into();
        let (another_id, new_preset) =
            build_search_preset_input(std::slice::from_ref(&custom), SearchAdapter::Exa);
        assert_ne!(another_id, id);
        let new_preset = new_preset.unwrap();
        assert!(!new_preset.enabled);
        let second = Connection {
            id: another_id.clone(),
            kind: ConnectionKind::Search,
            config: new_preset.config,
            ..Default::default()
        };
        let connections = [custom, second];
        assert_eq!(
            build_search_assignment_inputs(&connections, Some("missing")),
            [] as [ConnectionInput; 0]
        );
        let changes = build_search_assignment_inputs(&connections, Some(&another_id));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].id, id);
        assert!(!changes[0].enabled);
        assert_eq!(changes[1].id, another_id);
        assert!(changes[1].enabled);
    }

    #[test]
    fn sakura_and_voicevox_presets_preserve_expected_defaults() {
        let vv = build_voicevox_preset_input(4, 3, Some(1.1));
        assert_eq!(vv.id, "voicevox");
        assert_eq!(vv.expected_revision, 4);
        assert!(matches!(vv.config, Some(ConnectionConfig::Tts(_))));

        let sakura_tts = build_sakura_tts_preset_input(&[]);
        assert_eq!(sakura_tts.id, "sakura-tts");
        assert!(matches!(sakura_tts.config, Some(ConnectionConfig::Tts(_))));
    }
}
