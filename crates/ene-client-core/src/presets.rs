//! Pure connection preset input builders for search, LLM, voice, and embedding providers.

mod featured;

pub use featured::FeaturedPreset;

use crate::domain::{
    Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind,
    LlmConnection, LlmProvider, SearchConnection,
};

fn preset_endpoint(
    adapter: impl Into<String>,
    endpoint: impl Into<String>,
    options: impl Into<String>,
) -> ConnectionEndpoint {
    ConnectionEndpoint {
        adapter: adapter.into(),
        endpoint: endpoint.into(),
        options_schema: String::new(),
        options: options.into(),
    }
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
        && let Some(ConnectionConfig::Search(SearchConnection {
            endpoint: config, ..
        })) = &conn.config
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
            endpoint: preset_endpoint(provider.adapter_id, endpoint, "{}"),
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
            endpoint: preset_endpoint(provider.adapter, provider.endpoint, "{}"),
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
        config.endpoint.endpoint = "https://custom.example/".into();
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
    fn synthesis_presets_preserve_expected_defaults() {
        for (preset, adapter, voice, options) in [
            (
                FeaturedPreset::Voicevox,
                "voicevox",
                "3",
                serde_json::json!({"speaker": 3}),
            ),
            (
                FeaturedPreset::SakuraTts,
                "sakura",
                "zundamon/normal",
                serde_json::json!({
                    "protocol": "sakura", "model": "zundamon", "voice": "normal", "response_format": "wav",
                }),
            ),
            (
                FeaturedPreset::OpenAiTts,
                "openai",
                "alloy",
                serde_json::json!({"protocol": "openai", "model": "tts-1", "voice": "alloy"}),
            ),
        ] {
            let (_, input, _) = preset.prepare(&[]);
            let Some(ConnectionConfig::Tts(tts)) = input.config else {
                panic!("the preset provides a TTS config");
            };
            let endpoint = tts.endpoint;
            assert_eq!(endpoint.adapter, adapter);
            assert_eq!(tts.voice_ids, [voice]);
            assert_eq!(endpoint.options, options.to_string());
        }
    }
}
