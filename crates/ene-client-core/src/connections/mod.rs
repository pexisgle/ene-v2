//! Pure connection configuration editing, assignment builders, and MCP draft helpers.

pub mod edits;
pub mod form;
mod harness;
pub mod mcp;
mod options;

pub use edits::{
    ConnectionEditSpec, apply_connection_edits, build_custom_connection_input, decode_model_choice,
    encode_model_choice, extract_kind_list_init, format_kv_lines, parse_search_cap, split_comma,
    toggle_comma_item,
};
pub use harness::task_harness_choices;
pub use mcp::{
    McpExtraConfig, McpTargetResult, build_mcp_connection_input, mcp_http_target, mcp_local_target,
    mcp_npx_target, mcp_process_target, parse_mcp_kv, parse_process_args,
};

use crate::domain::{
    Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput, ConnectionKind,
};

use crate::state::Model;
use options::{Speaker, SpeechOptions, nonempty};

/// A resource the operator can choose on a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionResource {
    /// A language model that generates replies.
    ChatModel,
    /// A speech synthesizer's numeric speaker.
    Speaker,
    /// A speech synthesis voice identified by model and style.
    Voice,
    /// A model that transcribes audio.
    TranscriptionModel,
    /// A model that produces embeddings.
    EmbeddingModel,
}

impl ConnectionResource {
    /// Returns the resource this connection exposes for direct selection.
    pub fn for_connection(connection: &Connection) -> Option<Self> {
        Some(match connection.kind {
            ConnectionKind::Llm => Self::ChatModel,
            ConnectionKind::Tts if connection.is_sakura() => Self::Voice,
            ConnectionKind::Tts
                if connection
                    .endpoint()
                    .is_some_and(|endpoint| endpoint.adapter.eq_ignore_ascii_case("voicevox")) =>
            {
                Self::Speaker
            }
            ConnectionKind::Stt => Self::TranscriptionModel,
            ConnectionKind::Embedding => Self::EmbeddingModel,
            _ => return None,
        })
    }

    /// Lists candidate identifiers and display names using the connection's cached capabilities.
    pub fn choices(self, model: &Model, id: &str) -> Vec<(String, String)> {
        match self {
            Self::ChatModel => model.available_models_for_connection(id),
            Self::Speaker | Self::Voice => model.voice_choices_for_connection(id),
            Self::TranscriptionModel => model.stt_model_choices_for_connection(id),
            Self::EmbeddingModel => model.embedding_model_choices_for_connection(id),
        }
    }

    /// Resolves the selected identifier from editor input, endpoint options and the first candidate.
    pub fn selected(
        self,
        draft: &str,
        endpoint: Option<&ConnectionEndpoint>,
        first: Option<&str>,
    ) -> Option<String> {
        let first_draft = || draft.split(',').find_map(nonempty).map(str::to_owned);
        let first_choice = || first.map(str::to_owned);
        match self {
            Self::ChatModel => first_draft(),
            Self::Speaker => first_draft().or_else(|| configured_voicevox_speaker(endpoint)),
            Self::Voice => first_draft().or_else(first_choice),
            Self::TranscriptionModel => first_draft()
                .or_else(|| configured_stt_model(endpoint))
                .or_else(first_choice),
            Self::EmbeddingModel => nonempty(draft).map(str::to_owned).or_else(first_choice),
        }
    }
}

/// Applies TTS voice settings onto a domain `ConnectionConfig`.
pub fn apply_tts_voice_to_config(cfg: &mut ConnectionConfig, is_sakura: bool, voice_clean: &str) {
    let ConnectionConfig::Tts(tts) = cfg else {
        return;
    };
    tts.voice_ids = vec![voice_clean.to_owned()];
    let mut ep = tts.endpoint.take().unwrap_or_default();
    let mut opts = SpeechOptions::parse(&ep.options);
    if is_sakura {
        opts.set_sakura_voice(voice_clean);
    } else if let Ok(speaker) = voice_clean.parse::<u32>() {
        opts.speaker = Some(Speaker::Id(speaker.into()));
    } else {
        opts.voice = Some(voice_clean.to_owned());
    }
    ep.options = opts.json();
    tts.endpoint = Some(ep);
}

/// Applies STT model selection onto a domain `ConnectionConfig`.
pub fn apply_stt_model_to_config(cfg: &mut ConnectionConfig, model_clean: &str) {
    let ConnectionConfig::Stt(stt) = cfg else {
        return;
    };
    let mut ep = stt.endpoint.take().unwrap_or_default();
    let mut opts = SpeechOptions::parse(&ep.options);
    opts.model = Some(model_clean.to_owned());
    ep.options = opts.json();
    stt.endpoint = Some(ep);
}

/// Reads a configured STT model, leaving the fallback to the caller.
pub fn configured_stt_model(endpoint: Option<&ConnectionEndpoint>) -> Option<String> {
    let model = SpeechOptions::parse(&endpoint?.options).model?;
    nonempty(&model).map(str::to_owned)
}

/// Reads the numeric VOICEVOX speaker from the options of an endpoint.
pub fn configured_voicevox_speaker(endpoint: Option<&ConnectionEndpoint>) -> Option<String> {
    match SpeechOptions::parse(&endpoint?.options).speaker? {
        Speaker::Id(id) => Some(id.to_string()),
        Speaker::Name(_) => None,
    }
}

/// Resolves the effective STT model identifier configured on a connection.
pub fn stt_model_of_connection(conn: &Connection) -> String {
    if let Some(ConnectionConfig::Stt(stt)) = &conn.config
        && let Some(model) = configured_stt_model(stt.endpoint.as_ref())
    {
        return model;
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
    if let Some(voice) = SpeechOptions::parse(&ep.options).voice_id(sakura) {
        return Some(voice);
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
    use ene_domain::CredentialState::{Absent, Environment, Registered};

    #[test]
    fn connection_resources_preserve_configured_values_without_catalog_entries() {
        let speaker = tts_conn(
            "tts",
            true,
            "VOICEVOX",
            "",
            vec![],
            &serde_json::json!({"speaker": 7}),
        );
        let resource = ConnectionResource::for_connection(&speaker).unwrap();
        assert_eq!(resource, ConnectionResource::Speaker);
        assert_eq!(
            resource.selected("", speaker.endpoint(), None).as_deref(),
            Some("7")
        );
        assert_eq!(
            resource
                .selected(" , 3, 9", speaker.endpoint(), None)
                .as_deref(),
            Some("3")
        );

        let stt = stt_conn("stt", true, "openai", "", "stored-model");
        let resource = ConnectionResource::for_connection(&stt).unwrap();
        assert_eq!(resource, ConnectionResource::TranscriptionModel);
        assert_eq!(
            resource
                .selected("", stt.endpoint(), Some("fetched-model"))
                .as_deref(),
            Some("stored-model")
        );
        assert_eq!(
            resource
                .selected("draft-model", stt.endpoint(), None)
                .as_deref(),
            Some("draft-model")
        );
    }

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
        conn.credential_state = Registered;
        assert!(conn.can_start_chat(catalog));
        conn.last_check = Some(ConnectionCheck::default());
        assert!(!conn.can_start_chat(catalog));
    }

    #[test]
    fn search_connections_detect_key_requirements_for_exa_and_tavily() {
        let make_search = |adapter: &str, ep: &str, credential_state| Connection {
            id: format!("search-{adapter}"),
            name: format!("Web検索 ({adapter})"),
            enabled: true,
            revision: 1,
            credential_state,
            kind: ConnectionKind::Search,
            last_check: None,
            config: Some(ConnectionConfig::Search(SearchConnection {
                endpoint: Some(endpoint(adapter, ep, "{}")),
                max_results: 5,
            })),
        };

        let exa_absent = make_search("exa", "https://api.exa.ai/search", Absent);
        let exa_registered = make_search("exa", "https://api.exa.ai/search", Registered);
        let tavily_absent = make_search("tavily", "https://api.tavily.com/search", Absent);
        let tavily_env = make_search("tavily", "https://api.tavily.com/search", Environment);
        let mwmbl_absent = make_search("mwmbl", "https://api.mwmbl.org/api/v2/search/", Absent);

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
        let input = apply_connection_edits(
            &conn,
            &ConnectionEditSpec {
                name: "Embedding",
                endpoint_url: "https://api.openai.com/v1",
                process: None,
                list_raw: "multilingual-e5-large",
                dimensions_raw: None,
                options_json: None,
            },
        );
        let Some(ConnectionConfig::Embedding(emb)) = input.config.as_ref() else {
            panic!("expected Embedding config");
        };
        assert_eq!(emb.dimensions, 1024);
        assert_eq!(emb.model_id, "multilingual-e5-large");
    }
}
