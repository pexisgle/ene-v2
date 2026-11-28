//! Domain connection form editing and custom connection builder helpers.

use super::configured_stt_model;
use super::options::SpeechOptions;
use crate::domain::{
    AcpConnection, Connection, ConnectionConfig, ConnectionEndpoint, ConnectionInput,
    ConnectionKind, EmbeddingConnection, LlmConnection, McpConnection, SearchConnection,
    SttConnection, TtsConnection,
};
use std::collections::BTreeMap;

/// Input fields captured when saving edits to an existing connection row.
#[derive(Debug)]
pub struct ConnectionEditSpec<'a> {
    /// Display name shown to the operator.
    pub name: &'a str,
    /// Base URL as the operator typed it.
    pub endpoint_url: &'a str,
    /// Command and arguments, for a process-backed connection.
    pub process: Option<(&'a str, &'a [String])>,
    /// The selected resource, or the comma-separated list for a list field.
    pub list_raw: &'a str,
    /// Embedding width as typed, for an embedding connection.
    pub dimensions_raw: Option<&'a str>,
    /// Adapter options JSON as typed.
    pub options_json: Option<&'a str>,
}

/// Extracts the initial comma/space-delimited list string for a connection's edit field.
pub fn extract_kind_list_init(cfg: Option<&ConnectionConfig>) -> String {
    match cfg {
        Some(ConnectionConfig::Llm(c)) => c.model_ids.join(", "),
        Some(ConnectionConfig::Tts(c)) => c.voice_ids.join(", "),
        Some(ConnectionConfig::Stt(c)) => {
            configured_stt_model(Some(&c.endpoint)).unwrap_or_default()
        }
        Some(ConnectionConfig::Embedding(c)) => c.model_id.clone(),
        Some(ConnectionConfig::Mcp(c)) => c.allowed_tools.join(", "),
        Some(ConnectionConfig::Acp(c)) => c.args.join(" "),
        Some(ConnectionConfig::Search(c)) if c.max_results > 0 => c.max_results.to_string(),
        Some(ConnectionConfig::Search(_)) | None => String::new(),
    }
}

/// Formats an ordered key-value map as `KEY=VALUE` or `Name: Value` lines.
pub fn format_kv_lines(map: &BTreeMap<String, String>, sep: char) -> String {
    map.iter()
        .map(|(k, v)| match sep {
            ':' => format!("{k}: {v}"),
            _ => format!("{k}{sep}{v}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits a comma-separated string into trimmed non-empty owned strings.
pub fn split_comma(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Toggles an item inside a slice and returns the resulting comma-separated string.
pub fn toggle_comma_item(items: &[String], item: &str) -> String {
    let mut next = items.to_vec();
    if let Some(pos) = next.iter().position(|x| x == item) {
        next.remove(pos);
    } else {
        next.push(item.to_owned());
    }
    next.join(", ")
}

fn apply_tts_edits(tts: &mut TtsConnection, list: &str) {
    let ep = &mut tts.endpoint;
    let voices = split_comma(list);
    let sakura =
        ep.adapter.eq_ignore_ascii_case("sakura") || ene_catalog::is_sakura_endpoint(&ep.endpoint);
    if sakura && let Some(first) = voices.first() {
        let mut opts = SpeechOptions::parse(&ep.options);
        opts.set_sakura_voice(first);
        ep.options = opts.json();
    } else if ep.adapter.eq_ignore_ascii_case("voicevox")
        && let Some(first) = voices.first()
        && let Ok(speaker_num) = first.trim().parse::<u32>()
    {
        let mut opts = SpeechOptions::parse(&ep.options);
        opts.set("speaker", speaker_num);
        ep.options = opts.json();
    }
    tts.voice_ids = voices;
}

fn apply_stt_edits(stt: &mut SttConnection, list: &str) {
    let model = list.trim();
    if !model.is_empty() {
        let mut opts = SpeechOptions::parse(&stt.endpoint.options);
        opts.set("model", model);
        stt.endpoint.options = opts.json();
    }
}

fn default_embedding_model(endpoint: &ConnectionEndpoint) -> &'static str {
    if endpoint.adapter.eq_ignore_ascii_case("sakura")
        || ene_catalog::is_sakura_endpoint(&endpoint.endpoint)
    {
        "multilingual-e5-large"
    } else {
        "text-embedding-3-small"
    }
}

fn apply_embedding_edits(emb: &mut EmbeddingConnection, spec: &ConnectionEditSpec<'_>) {
    if emb.endpoint.adapter.trim().is_empty() {
        emb.endpoint.adapter = "openai".to_owned();
    }
    let model_id = spec.list_raw.trim();
    let model_id = if model_id.is_empty() {
        default_embedding_model(&emb.endpoint)
    } else {
        model_id
    };
    let model_changed = emb.model_id != model_id;
    emb.model_id = model_id.to_owned();
    let parsed_dim = spec
        .dimensions_raw
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|&d| d > 0);
    if let Some(dims) = parsed_dim {
        emb.dimensions = dims;
    } else if emb.dimensions == 0 || model_changed {
        emb.dimensions = ene_catalog::default_embedding_dimensions(&emb.model_id);
    }
}

fn apply_mcp_edits(mcp: &mut McpConnection, spec: &ConnectionEditSpec<'_>) {
    mcp.endpoint.adapter = "mcp".to_owned();
    if let Some((cmd, args)) = spec.process {
        let trimmed = cmd.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            mcp.endpoint.endpoint = trimmed.to_owned();
            mcp.command.clear();
            mcp.args.clear();
        } else {
            mcp.command = trimmed.to_owned();
            mcp.args = args.to_vec();
        }
    }
    mcp.allowed_tools = split_comma(spec.list_raw);
}

/// A blank Search limit has no connection-specific cap.
pub fn parse_search_cap(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(0);
    }
    raw.parse().ok().filter(|&cap| (1..=20).contains(&cap))
}

fn apply_search_edits(search: &mut SearchConnection, list: &str) {
    if search.endpoint.adapter.trim().is_empty() {
        search.endpoint.adapter = "mwmbl".to_owned();
    }
    if let Some(cap) = parse_search_cap(list) {
        search.max_results = cap;
    }
}

/// Applies edited connection fields to produce a `ConnectionInput` update payload.
pub fn apply_connection_edits(conn: &Connection, spec: &ConnectionEditSpec<'_>) -> ConnectionInput {
    let mut input = conn.to_input();
    input.name = spec.name.to_owned();
    let Some(config) = input.config.as_mut() else {
        return input;
    };
    if !matches!(config, ConnectionConfig::Acp(_)) {
        let endpoint = config.endpoint_mut();
        endpoint.endpoint = spec.endpoint_url.to_owned();
        if let Some(options) = spec.options_json {
            endpoint.options = options.to_owned();
        }
    }
    match config {
        ConnectionConfig::Llm(llm) => llm.model_ids = split_comma(spec.list_raw),
        ConnectionConfig::Tts(tts) => apply_tts_edits(tts, spec.list_raw),
        ConnectionConfig::Stt(stt) => apply_stt_edits(stt, spec.list_raw),
        ConnectionConfig::Embedding(emb) => apply_embedding_edits(emb, spec),
        ConnectionConfig::Mcp(mcp) => apply_mcp_edits(mcp, spec),
        ConnectionConfig::Search(search) => apply_search_edits(search, spec.list_raw),
        ConnectionConfig::Acp(_) => {}
    }

    input
}

/// Builds a new custom `ConnectionInput` for non-MCP connection kinds.
pub fn build_custom_connection_input(
    kind: ConnectionKind,
    name: &str,
    adapter: &str,
    endpoint: &str,
) -> Option<ConnectionInput> {
    let ep = ConnectionEndpoint {
        adapter: adapter.to_owned(),
        endpoint: endpoint.to_owned(),
        options_schema: String::new(),
        options: "{}".into(),
    };
    let config = match kind {
        ConnectionKind::Llm => ConnectionConfig::Llm(LlmConnection {
            endpoint: ep,
            model_ids: Vec::new(),
        }),
        ConnectionKind::Tts => ConnectionConfig::Tts(TtsConnection {
            endpoint: ep,
            voice_ids: Vec::new(),
        }),
        ConnectionKind::Stt => ConnectionConfig::Stt(SttConnection {
            endpoint: ep,
            language_hints: Vec::new(),
        }),
        ConnectionKind::Embedding => {
            let mut emb_ep = ep;
            if emb_ep.adapter.trim().is_empty() {
                emb_ep.adapter = "openai".to_owned();
            }
            let model_id = default_embedding_model(&emb_ep).to_owned();
            let dimensions = ene_catalog::default_embedding_dimensions(&model_id);
            ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: emb_ep,
                model_id,
                dimensions,
            })
        }
        ConnectionKind::Mcp | ConnectionKind::Unspecified => return None,
        ConnectionKind::Search => {
            let mut search_ep = ep;
            if search_ep.adapter.trim().is_empty() {
                search_ep.adapter = "mwmbl".to_owned();
            }
            ConnectionConfig::Search(SearchConnection {
                endpoint: search_ep,
                max_results: 5,
            })
        }
        ConnectionKind::Acp => ConnectionConfig::Acp(AcpConnection {
            endpoint: ConnectionEndpoint {
                endpoint: String::new(),
                ..ep
            },
            ..Default::default()
        }),
    };
    Some(ConnectionInput {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
        enabled: !matches!(config, ConnectionConfig::Search(_)),
        expected_revision: 0,
        config: Some(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::mcp::parse_process_args;

    #[test]
    fn selecting_a_transcription_model_preserves_the_saved_language_hints() {
        let endpoint = ConnectionEndpoint {
            adapter: "openai".into(),
            options: r#"{"model":"old","language":"ja","custom":true,"speaker":{"preset":1}}"#
                .into(),
            ..Default::default()
        };
        let conn = Connection {
            kind: ConnectionKind::Stt,
            config: Some(ConnectionConfig::Stt(SttConnection {
                endpoint,
                language_hints: vec!["ja".into(), "en".into()],
            })),
            ..Default::default()
        };
        assert_eq!(extract_kind_list_init(conn.config.as_ref()), "old");
        let input = apply_connection_edits(
            &conn,
            &ConnectionEditSpec {
                name: "Transcription",
                endpoint_url: "",
                process: None,
                list_raw: "new",
                dimensions_raw: None,
                options_json: None,
            },
        );
        let Some(ConnectionConfig::Stt(stt)) = input.config else {
            panic!("expected STT")
        };
        assert_eq!(stt.language_hints, ["ja", "en"]);
        let options: serde_json::Value = serde_json::from_str(&stt.endpoint.options).unwrap();
        assert_eq!(
            options,
            serde_json::json!({"model":"new","language":"ja","custom":true,"speaker":{"preset":1}})
        );
    }

    #[test]
    fn changes_to_other_search_fields_preserve_a_connection_without_a_cap() {
        let conn = Connection {
            kind: ConnectionKind::Search,
            config: Some(ConnectionConfig::Search(SearchConnection::default())),
            ..Default::default()
        };
        let initial = extract_kind_list_init(conn.config.as_ref());
        assert_eq!(initial, "");
        let update = apply_connection_edits(
            &conn,
            &ConnectionEditSpec {
                name: "My search",
                endpoint_url: "",
                process: None,
                list_raw: &initial,
                dimensions_raw: None,
                options_json: None,
            },
        );
        let Some(ConnectionConfig::Search(search)) = update.config else {
            panic!("expected updated search settings");
        };
        assert_eq!(search.max_results, 0);
        assert_eq!(parse_search_cap("20"), Some(20));
        assert_eq!(parse_search_cap("21"), None);
    }

    #[test]
    fn editing_a_saved_mcp_connection_replaces_only_the_requested_arguments() {
        let mcp = McpConnection {
            command: "npx".into(),
            args: ["-y", "old-package", r"C:\My Tools\日本語", ""]
                .map(str::to_owned)
                .to_vec(),
            allowed_tools: vec!["echo".into()],
            ..Default::default()
        };
        let conn = Connection {
            id: "saved-mcp".into(),
            kind: ConnectionKind::Mcp,
            revision: 3,
            config: Some(ConnectionConfig::Mcp(mcp.clone())),
            ..Default::default()
        };
        let initial = serde_json::json!(mcp.args).to_string();
        let draft = initial.replace("old-package", "new-package");
        let args = parse_process_args(&draft).unwrap();
        let input = apply_connection_edits(
            &conn,
            &ConnectionEditSpec {
                name: "Saved MCP",
                endpoint_url: "",
                process: Some(("npx", &args)),
                list_raw: "echo",
                dimensions_raw: None,
                options_json: None,
            },
        );
        assert_eq!(input.expected_revision, conn.revision);
        let Some(ConnectionConfig::Mcp(saved)) = input.config.as_ref() else {
            panic!("expected MCP settings");
        };
        assert_eq!(saved.command, "npx");
        assert_eq!(saved.args, ["-y", "new-package", r"C:\My Tools\日本語", ""]);
        assert_eq!(saved.allowed_tools, mcp.allowed_tools);
    }
}
