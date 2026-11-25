//! Pure connection configuration editing, assignment builders, and MCP draft helpers.

pub mod edits;
pub mod mcp;

pub use edits::{
    ConnectionEditSpec, apply_connection_edits, apply_mcp_extra_edits,
    build_custom_connection_input, decode_model_choice, encode_model_choice,
    extract_kind_list_init, format_kv_lines, parse_kv_lines, split_comma, toggle_comma_item,
};
pub use mcp::{
    McpExtraConfig, McpTargetResult, build_mcp_connection_input, mcp_http_target, mcp_local_target,
    mcp_npx_target, mcp_process_target, parse_mcp_kv, parse_process_args,
};

use crate::domain::{
    Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind,
};

pub(crate) fn parse_options_map(raw: &str) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// Applies TTS voice settings onto a domain `ConnectionConfig`.
pub fn apply_tts_voice_to_config(cfg: &mut ConnectionConfig, is_sakura: bool, voice_clean: &str) {
    let ConnectionConfig::Tts(tts) = cfg else {
        return;
    };
    tts.voice_ids = vec![voice_clean.to_owned()];
    let mut ep = tts.endpoint.take().unwrap_or_default();
    let mut opts = parse_options_map(&ep.options);
    if is_sakura {
        if let Some((model, voice)) = voice_clean.split_once('/') {
            if !model.trim().is_empty() {
                opts.insert("model".into(), serde_json::json!(model.trim()));
            }
            if !voice.trim().is_empty() {
                opts.insert("voice".into(), serde_json::json!(voice.trim()));
            }
        } else {
            opts.insert("model".into(), serde_json::json!(voice_clean));
        }
        opts.entry("protocol".to_owned())
            .or_insert_with(|| serde_json::json!("sakura"));
        opts.entry("response_format".to_owned())
            .or_insert_with(|| serde_json::json!("wav"));
    } else if let Ok(speaker_num) = voice_clean.parse::<u32>() {
        opts.insert("speaker".into(), serde_json::json!(speaker_num));
    } else {
        opts.insert("voice".into(), serde_json::json!(voice_clean));
    }
    ep.options = serde_json::Value::Object(opts).to_string();
    tts.endpoint = Some(ep);
}

/// Applies STT model selection onto a domain `ConnectionConfig`.
pub fn apply_stt_model_to_config(cfg: &mut ConnectionConfig, model_clean: &str) {
    let ConnectionConfig::Stt(stt) = cfg else {
        return;
    };
    let mut ep = stt.endpoint.take().unwrap_or_default();
    let mut opts = parse_options_map(&ep.options);
    opts.insert("model".into(), serde_json::json!(model_clean));
    ep.options = serde_json::Value::Object(opts).to_string();
    stt.endpoint = Some(ep);
}

/// Resolves the effective STT model identifier configured on a connection.
pub fn stt_model_of_connection(conn: &Connection) -> String {
    if let Some(ConnectionConfig::Stt(stt)) = &conn.config
        && let Some(ep) = &stt.endpoint
        && let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options)
        && let Some(model) = opts
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return model.to_owned();
    }
    if conn.is_sakura() {
        "whisper-large-v3-turbo".to_owned()
    } else {
        "whisper-1".to_owned()
    }
}

/// Resolves the effective TTS voice identifier configured on a connection.
pub fn tts_voice_of_connection(conn: &Connection) -> Option<String> {
    let Some(ConnectionConfig::Tts(tts)) = &conn.config else {
        return None;
    };
    if let Some(first) = tts
        .voice_ids
        .first()
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (vid, _) = ene_catalog::parse_voice_resource(first);
        if !vid.is_empty() {
            return Some(vid);
        }
    }
    let sakura = conn.is_sakura();
    let ep = tts.endpoint.as_ref()?;
    tts_voice_from_endpoint(ep, sakura)
}

fn tts_voice_from_endpoint(ep: &ConnectionEndpoint, sakura: bool) -> Option<String> {
    if let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options) {
        if sakura {
            let model = opts
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("zundamon");
            let voice = opts
                .get("voice")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("normal");
            return Some(format!("{model}/{voice}"));
        }
        if let Some(speaker) = opts.get("speaker") {
            if let Some(num) = speaker.as_u64() {
                return Some(num.to_string());
            }
            if let Some(s) = speaker.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(s.to_owned());
            }
        }
        if let Some(voice) = opts
            .get("voice")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(voice.to_owned());
        }
    }
    if sakura {
        return Some("zundamon/normal".to_owned());
    }
    match ep.adapter.trim().to_ascii_lowercase().as_str() {
        "voicevox" => Some("3".to_owned()),
        "openai" => Some("alloy".to_owned()),
        _ => None,
    }
}

/// Returns the first enabled STT `(connection_id, model_id)`.
pub fn active_stt_in_connections(connections: &[Connection]) -> Option<(String, String)> {
    connections
        .iter()
        .find(|c| c.kind == ConnectionKind::Stt && c.enabled)
        .map(|c| (c.id.clone(), stt_model_of_connection(c)))
}

/// Returns the first enabled TTS `(connection_id, voice_id)`.
pub fn active_tts_in_connections(connections: &[Connection]) -> Option<(String, String)> {
    connections
        .iter()
        .find(|c| c.kind == ConnectionKind::Tts && c.enabled)
        .and_then(|c| tts_voice_of_connection(c).map(|v| (c.id.clone(), v)))
}

/// Builds a `ConnectionInput` disabling an existing connection while preserving its config.
pub fn disable_connection_input(conn: &Connection) -> ConnectionInput {
    let mut input = conn.to_input();
    input.enabled = false;
    input
}

fn build_voice_assignment_inputs<F>(
    connections: &[Connection],
    kind: ConnectionKind,
    selection: Option<(&str, &str)>,
    apply_cfg: F,
) -> Vec<ConnectionInput>
where
    F: Fn(&Connection, &mut ConnectionConfig, &str),
{
    let mut inputs = Vec::new();
    match selection {
        Some((target_id, value)) => {
            let clean = value.trim();
            if clean.is_empty()
                || !connections
                    .iter()
                    .any(|c| c.id == target_id && c.kind == kind)
            {
                return inputs;
            }
            for conn in connections.iter().filter(|c| c.kind == kind) {
                if conn.id == target_id {
                    let mut input = conn.to_input();
                    input.enabled = true;
                    if let Some(cfg) = input.config.as_mut() {
                        apply_cfg(conn, cfg, clean);
                    }
                    inputs.push(input);
                } else if conn.enabled {
                    inputs.push(disable_connection_input(conn));
                }
            }
        }
        None => {
            for conn in connections.iter().filter(|c| c.kind == kind && c.enabled) {
                inputs.push(disable_connection_input(conn));
            }
        }
    }
    inputs
}

/// Builds atomic settings inputs activating the selected STT connection/model and disabling others.
pub fn build_stt_assignment_inputs(
    connections: &[Connection],
    selection: Option<(&str, &str)>,
) -> Vec<ConnectionInput> {
    build_voice_assignment_inputs(
        connections,
        ConnectionKind::Stt,
        selection,
        |_, cfg, model| {
            apply_stt_model_to_config(cfg, model);
        },
    )
}

/// Builds atomic settings inputs activating the selected TTS connection/voice and disabling others.
pub fn build_tts_assignment_inputs(
    connections: &[Connection],
    selection: Option<(&str, &str)>,
) -> Vec<ConnectionInput> {
    build_voice_assignment_inputs(
        connections,
        ConnectionKind::Tts,
        selection,
        |conn, cfg, v| {
            apply_tts_voice_to_config(cfg, conn.is_sakura(), v);
        },
    )
}

/// Builds a `ConnectionInput` updating the selected model ID on an LLM connection.
pub fn build_connection_model_input(conn: &Connection, model_id: &str) -> ConnectionInput {
    let mut input = conn.to_input();
    if let Some(ConnectionConfig::Llm(llm)) = input.config.as_mut() {
        llm.model_ids = vec![model_id.to_owned()];
    }
    input
}

/// Builds a `ConnectionInput` updating the embedding model ID and default dimensions.
pub fn build_embedding_model_input(conn: &Connection, model_id: &str) -> ConnectionInput {
    let mut input = conn.to_input();
    if let Some(ConnectionConfig::Embedding(emb)) = input.config.as_mut() {
        emb.model_id = model_id.to_owned();
        emb.dimensions = ene_catalog::default_embedding_dimensions(model_id);
    }
    input
}

/// Builds a `ConnectionInput` toggling `enabled` while keeping existing configuration intact.
pub fn build_connection_enabled_input(conn: &Connection, enabled: bool) -> ConnectionInput {
    let mut input = conn.to_input();
    input.enabled = enabled;
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ConnectionCheck, EmbeddingConnection, LlmConnection, LlmProvider, SearchConnection,
        SttConnection, TtsConnection,
    };

    fn endpoint(adapter: &str, endpoint: &str, options: &str) -> ConnectionEndpoint {
        ConnectionEndpoint {
            adapter: adapter.into(),
            endpoint: endpoint.into(),
            options: options.into(),
            ..Default::default()
        }
    }

    fn stt_conn(id: &str, enabled: bool, adapter: &str, ep: &str, model: &str) -> Connection {
        Connection {
            id: id.into(),
            name: id.into(),
            enabled,
            revision: 3,
            kind: ConnectionKind::Stt,
            last_check: None,
            config: Some(ConnectionConfig::Stt(SttConnection {
                endpoint: Some(endpoint(
                    adapter,
                    ep,
                    &serde_json::json!({ "model": model }).to_string(),
                )),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn tts_conn(
        id: &str,
        enabled: bool,
        adapter: &str,
        ep: &str,
        voices: Vec<&str>,
        options: &serde_json::Value,
    ) -> Connection {
        Connection {
            id: id.into(),
            name: id.into(),
            enabled,
            revision: 5,
            kind: ConnectionKind::Tts,
            last_check: None,
            config: Some(ConnectionConfig::Tts(TtsConnection {
                endpoint: Some(endpoint(adapter, ep, &options.to_string())),
                voice_ids: voices.into_iter().map(str::to_owned).collect(),
            })),
            ..Default::default()
        }
    }

    #[test]
    fn active_stt_and_tts_selection_reads_first_enabled_connection() {
        let sakura = ene_catalog::SAKURA_AI_ENDPOINT;
        let connections = vec![
            stt_conn(
                "stt-disabled",
                false,
                "openai",
                "https://api.openai.com/v1",
                "whisper-1",
            ),
            stt_conn(
                "stt-sakura",
                true,
                "sakura",
                sakura,
                "whisper-large-v3-turbo",
            ),
            tts_conn(
                "tts-voicevox",
                false,
                "voicevox",
                "http://127.0.0.1:50021",
                vec![],
                &serde_json::json!({ "speaker": 3 }),
            ),
            tts_conn(
                "tts-sakura",
                true,
                "sakura",
                sakura,
                vec!["zundamon/amaama"],
                &serde_json::json!({ "protocol": "sakura", "model": "zundamon", "voice": "amaama" }),
            ),
        ];
        assert_eq!(
            active_stt_in_connections(&connections),
            Some(("stt-sakura".into(), "whisper-large-v3-turbo".into()))
        );
        assert_eq!(
            active_tts_in_connections(&connections),
            Some(("tts-sakura".into(), "zundamon/amaama".into()))
        );
    }

    #[test]
    fn stt_assignment_inputs_activates_target_and_disables_others() {
        let sakura = ene_catalog::SAKURA_AI_ENDPOINT;
        let connections = vec![
            stt_conn(
                "stt-openai",
                true,
                "openai",
                "https://api.openai.com/v1",
                "whisper-1",
            ),
            stt_conn(
                "stt-sakura",
                false,
                "sakura",
                sakura,
                "whisper-large-v3-turbo",
            ),
        ];

        let inputs =
            build_stt_assignment_inputs(&connections, Some(("stt-sakura", "gpt-4o-transcribe")));
        assert_eq!(inputs.len(), 2);
        let disabled = inputs
            .iter()
            .find(|i| i.id == "stt-openai")
            .expect("old disabled");
        assert!(!disabled.enabled);
        assert_eq!(disabled.expected_revision, 3);

        let activated = inputs
            .iter()
            .find(|i| i.id == "stt-sakura")
            .expect("target enabled");
        assert!(activated.enabled);
        let Some(ConnectionConfig::Stt(stt)) = activated.config.as_ref() else {
            panic!("expected Stt config");
        };
        let opts: serde_json::Value =
            serde_json::from_str(&stt.endpoint.as_ref().expect("endpoint").options)
                .expect("json options");
        assert_eq!(opts["model"], "gpt-4o-transcribe");

        let off_inputs = build_stt_assignment_inputs(&connections, None);
        assert_eq!(off_inputs.len(), 1);
        assert_eq!(off_inputs[0].id, "stt-openai");
        assert!(!off_inputs[0].enabled);
    }

    #[test]
    fn tts_assignment_inputs_activates_target_updates_voice_and_disables_others() {
        let sakura = ene_catalog::SAKURA_AI_ENDPOINT;
        let connections = vec![
            tts_conn(
                "tts-voicevox",
                true,
                "voicevox",
                "http://127.0.0.1:50021",
                vec!["3"],
                &serde_json::json!({ "speaker": 3 }),
            ),
            tts_conn(
                "tts-sakura",
                false,
                "sakura",
                sakura,
                vec!["zundamon/normal"],
                &serde_json::json!({ "protocol": "sakura", "model": "zundamon", "voice": "normal" }),
            ),
        ];

        let inputs =
            build_tts_assignment_inputs(&connections, Some(("tts-sakura", "shikokumetan/amaama")));
        assert_eq!(inputs.len(), 2);
        let disabled = inputs
            .iter()
            .find(|i| i.id == "tts-voicevox")
            .expect("old disabled");
        assert!(!disabled.enabled);
        assert_eq!(disabled.expected_revision, 5);

        let activated = inputs
            .iter()
            .find(|i| i.id == "tts-sakura")
            .expect("target enabled");
        assert!(activated.enabled);
        let Some(ConnectionConfig::Tts(tts)) = activated.config.as_ref() else {
            panic!("expected Tts config");
        };
        assert_eq!(tts.voice_ids, vec!["shikokumetan/amaama"]);
        let opts: serde_json::Value =
            serde_json::from_str(&tts.endpoint.as_ref().expect("endpoint").options)
                .expect("json options");
        assert_eq!(opts["model"], "shikokumetan");
        assert_eq!(opts["voice"], "amaama");
        assert_eq!(opts["protocol"], "sakura");
    }

    #[test]
    fn a_public_model_list_does_not_make_an_unregistered_connection_ready() {
        let provider = LlmProvider {
            requires_credentials: true,
            adapter: "openrouter".into(),
            endpoint: "https://openrouter.ai/api/v1".into(),
            ..Default::default()
        };
        let mut conn = Connection {
            enabled: true,
            kind: ConnectionKind::Llm,
            last_check: Some(ConnectionCheck {
                healthy: true,
                ..Default::default()
            }),
            config: Some(ConnectionConfig::Llm(LlmConnection {
                endpoint: Some(endpoint("openrouter", "https://openrouter.ai/api/v1", "{}")),
                ..Default::default()
            })),
            ..Default::default()
        };
        let catalog = std::slice::from_ref(&provider);
        assert!(!conn.can_start_chat(catalog));
        conn.credential_state = "registered".into();
        assert!(conn.can_start_chat(catalog));
        conn.last_check = Some(ConnectionCheck::default());
        assert!(!conn.can_start_chat(catalog));
    }

    #[test]
    fn search_connections_detect_key_requirements_for_exa_and_tavily() {
        let make_search = |adapter: &str, ep: &str, cred_state: &str| Connection {
            id: format!("search-{adapter}"),
            name: format!("Web検索 ({adapter})"),
            enabled: true,
            revision: 1,
            credential_state: cred_state.into(),
            kind: ConnectionKind::Search,
            last_check: None,
            config: Some(ConnectionConfig::Search(SearchConnection {
                endpoint: Some(endpoint(adapter, ep, "{}")),
                max_results: 5,
            })),
        };

        let exa_absent = make_search("exa", "https://api.exa.ai/search", "absent");
        let exa_registered = make_search("exa", "https://api.exa.ai/search", "registered");
        let tavily_absent = make_search("tavily", "https://api.tavily.com/search", "absent");
        let tavily_env = make_search("tavily", "https://api.tavily.com/search", "environment");
        let mwmbl_absent = make_search("mwmbl", "https://api.mwmbl.org/api/v2/search/", "absent");

        assert_eq!(exa_absent.search_adapter(), Some("exa"));
        assert_eq!(tavily_absent.search_adapter(), Some("tavily"));
        assert!(exa_absent.needs_key(&[]));
        assert!(!exa_registered.needs_key(&[]));
        assert!(tavily_absent.needs_key(&[]));
        assert!(!tavily_env.needs_key(&[]));
        assert!(!mwmbl_absent.needs_key(&[]));
    }

    #[test]
    fn embedding_model_inputs_pick_the_catalog_default_dimensions() {
        let conn = Connection {
            id: "emb".into(),
            enabled: true,
            kind: ConnectionKind::Embedding,
            config: Some(ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: Some(endpoint("openai", "https://api.openai.com/v1", "{}")),
                model_id: "text-embedding-3-small".into(),
                dimensions: 1536,
            })),
            ..Default::default()
        };
        let input = build_embedding_model_input(&conn, "multilingual-e5-large");
        let Some(ConnectionConfig::Embedding(emb)) = input.config.as_ref() else {
            panic!("expected Embedding config");
        };
        assert_eq!(emb.dimensions, 1024);
        assert_eq!(emb.model_id, "multilingual-e5-large");
    }
}
