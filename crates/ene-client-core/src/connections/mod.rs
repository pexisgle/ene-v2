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
