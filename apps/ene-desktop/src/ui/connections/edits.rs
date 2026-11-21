use crate::ui::{default_embedding_dimensions, is_sakura_endpoint};
use ene_rpc as w;
use std::collections::HashMap;
use std::hash::BuildHasher;

pub(super) struct ConnectionEditSpec<'a> {
    pub(super) name: &'a str,
    pub(super) endpoint_url: &'a str,
    pub(super) process: Option<(&'a str, &'a [String])>,
    pub(super) list_raw: &'a str,
    pub(super) dimensions_raw: Option<&'a str>,
    pub(super) options_json: Option<&'a str>,
}

pub(super) fn encode_model_choice(connection_id: &str, model_id: &str) -> String {
    format!("{connection_id}\u{1f}{model_id}")
}

pub(super) fn decode_model_choice(encoded: &str) -> Option<(String, String)> {
    let (cid, mid) = encoded.split_once('\u{1f}')?;
    if cid.is_empty() || mid.is_empty() {
        return None;
    }
    Some((cid.to_owned(), mid.to_owned()))
}

pub(super) fn kind_label(kind: w::ConnectionKind) -> &'static str {
    match kind {
        w::ConnectionKind::Llm => "LLM",
        w::ConnectionKind::Tts => "音声合成",
        w::ConnectionKind::Stt => "音声認識",
        w::ConnectionKind::Embedding => "埋め込み",
        w::ConnectionKind::Mcp => "MCP",
        w::ConnectionKind::Acp => "ACP",
        w::ConnectionKind::Search => "Web検索",
        w::ConnectionKind::Unspecified => "その他",
    }
}

pub(super) fn kind_of(conn: &w::Connection) -> w::ConnectionKind {
    conn.kind
        .as_known()
        .unwrap_or(w::ConnectionKind::Unspecified)
}

pub(super) fn extract_endpoint(
    cfg: Option<&w::connection_config::Config>,
) -> Option<w::ConnectionEndpoint> {
    match cfg? {
        w::connection_config::Config::Llm(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Tts(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Stt(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Embedding(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Mcp(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Acp(c) => c.endpoint.as_option().cloned(),
        w::connection_config::Config::Search(c) => c.endpoint.as_option().cloned(),
    }
}

pub(super) fn extract_kind_list_init(cfg: Option<&w::connection_config::Config>) -> String {
    match cfg {
        Some(w::connection_config::Config::Llm(c)) => c.model_ids.join(", "),
        Some(w::connection_config::Config::Tts(c)) => c.voice_ids.join(", "),
        Some(w::connection_config::Config::Stt(c)) => c.language_hints.join(", "),
        Some(w::connection_config::Config::Embedding(c)) => c.model_id.clone(),
        Some(w::connection_config::Config::Mcp(c)) => c.allowed_tools.join(", "),
        Some(w::connection_config::Config::Acp(c)) => c.args.join(" "),
        Some(w::connection_config::Config::Search(c)) => {
            if c.max_results == 0 {
                "5".into()
            } else {
                c.max_results.to_string()
            }
        }
        None => String::new(),
    }
}

pub(super) fn format_kv_lines<S: BuildHasher>(
    map: &HashMap<String, String, S>,
    sep: char,
) -> String {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort_by_key(|(a, _)| *a);
    pairs
        .into_iter()
        .map(|(k, v)| match sep {
            ':' => format!("{k}: {v}"),
            _ => format!("{k}{sep}{v}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn parse_kv_lines(raw: &str, sep: char) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once(sep) else {
            return Err(match sep {
                ':' => "ヘッダーは `Name: Value` 形式で入力してください。",
                _ => "環境変数は `KEY=VALUE` 形式で入力してください。",
            });
        };
        let key = k.trim();
        if key.is_empty() {
            return Err("空のキーは指定できません。");
        }
        out.push((key.to_owned(), v.trim().to_owned()));
    }
    Ok(out)
}

pub(super) fn apply_mcp_extra_edits(
    input: &mut w::ConnectionInput,
    cwd: &str,
    secret_env_var: &str,
    auth_header: &str,
    env: &[(String, String)],
    headers: &[(String, String)],
) {
    let mut cfg = input.config.as_option().cloned().unwrap_or_default();
    if let Some(w::connection_config::Config::Mcp(mcp)) = cfg.config.as_mut() {
        mcp.cwd = cwd.to_owned();
        mcp.secret_env_var = secret_env_var.to_owned();
        mcp.auth_header = auth_header.to_owned();
        mcp.env = env.iter().cloned().collect();
        mcp.headers = headers.iter().cloned().collect();
    }
    input.config = Some(cfg).into();
}

pub(super) fn split_comma(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn toggle_comma_item(items: &[String], item: &str) -> String {
    let mut next = items.to_vec();
    if let Some(pos) = next.iter().position(|x| x == item) {
        next.remove(pos);
    } else {
        next.push(item.to_owned());
    }
    next.join(", ")
}

fn updated_endpoint(
    cur: Option<&w::ConnectionEndpoint>,
    endpoint_url: &str,
    options_json: Option<&str>,
) -> w::ConnectionEndpoint {
    let mut updated = cur.cloned().unwrap_or_default();
    updated.endpoint = endpoint_url.to_owned();
    if let Some(opts) = options_json {
        updated.options = opts.to_owned();
    }
    updated
}

fn parse_endpoint_options_map(raw: &str) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

fn apply_tts_edits(tts: &mut w::TtsConnection, spec: &ConnectionEditSpec<'_>) {
    let mut ep = updated_endpoint(
        tts.endpoint.as_option(),
        spec.endpoint_url,
        spec.options_json,
    );
    let voices = split_comma(spec.list_raw);
    let sakura = ep.adapter.eq_ignore_ascii_case("sakura") || is_sakura_endpoint(&ep.endpoint);
    if sakura && let Some(first) = voices.first() {
        let mut opts = parse_endpoint_options_map(&ep.options);
        if let Some((m, v)) = first.split_once('/') {
            if !m.trim().is_empty() {
                opts.insert("model".into(), serde_json::json!(m.trim()));
            }
            if !v.trim().is_empty() {
                opts.insert("voice".into(), serde_json::json!(v.trim()));
            }
        } else if !first.trim().is_empty() {
            opts.insert("model".into(), serde_json::json!(first.trim()));
        }
        opts.entry("protocol".to_owned())
            .or_insert_with(|| serde_json::json!("sakura"));
        opts.entry("response_format".to_owned())
            .or_insert_with(|| serde_json::json!("wav"));
        ep.options = serde_json::Value::Object(opts).to_string();
    } else if ep.adapter.eq_ignore_ascii_case("voicevox")
        && let Some(first) = voices.first()
        && let Ok(speaker_num) = first.trim().parse::<u32>()
    {
        let mut opts = parse_endpoint_options_map(&ep.options);
        opts.insert("speaker".into(), serde_json::json!(speaker_num));
        ep.options = serde_json::Value::Object(opts).to_string();
    }
    tts.endpoint = Some(ep).into();
    tts.voice_ids = voices;
}

fn apply_stt_edits(stt: &mut w::SttConnection, spec: &ConnectionEditSpec<'_>) {
    let mut ep = updated_endpoint(
        stt.endpoint.as_option(),
        spec.endpoint_url,
        spec.options_json,
    );
    let hints = split_comma(spec.list_raw);
    if let Some(first_lang) = hints.first() {
        let mut opts = parse_endpoint_options_map(&ep.options);
        opts.insert("language".into(), serde_json::json!(first_lang));
        ep.options = serde_json::Value::Object(opts).to_string();
    }
    stt.endpoint = Some(ep).into();
    stt.language_hints = hints;
}

fn apply_embedding_edits(emb: &mut w::EmbeddingConnection, spec: &ConnectionEditSpec<'_>) {
    let mut ep = updated_endpoint(
        emb.endpoint.as_option(),
        spec.endpoint_url,
        spec.options_json,
    );
    if ep.adapter.trim().is_empty() {
        ep.adapter = "openai".to_owned();
    }
    let sakura = ep.adapter.eq_ignore_ascii_case("sakura") || is_sakura_endpoint(&ep.endpoint);
    emb.endpoint = Some(ep).into();
    let trimmed = spec.list_raw.trim();
    emb.model_id = if trimmed.is_empty() {
        if sakura {
            "multilingual-e5-large".to_owned()
        } else {
            "text-embedding-3-small".to_owned()
        }
    } else {
        trimmed.to_owned()
    };
    let parsed_dim = spec
        .dimensions_raw
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|&d| d > 0);
    if let Some(dims) = parsed_dim {
        emb.dimensions = dims;
    } else if emb.dimensions == 0 {
        emb.dimensions = default_embedding_dimensions(&emb.model_id);
    }
}

fn apply_mcp_edits(mcp: &mut w::McpConnection, spec: &ConnectionEditSpec<'_>) {
    let mut ep = updated_endpoint(
        mcp.endpoint.as_option(),
        spec.endpoint_url,
        spec.options_json,
    );
    ep.adapter = "mcp".to_owned();
    if let Some((cmd, args)) = spec.process {
        let trimmed = cmd.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            ep.endpoint = trimmed.to_owned();
            mcp.command.clear();
            mcp.args.clear();
        } else {
            mcp.command = trimmed.to_owned();
            mcp.args = args.to_vec();
        }
    }
    mcp.endpoint = Some(ep).into();
    mcp.allowed_tools = split_comma(spec.list_raw);
}

fn apply_search_edits(search: &mut w::SearchConnection, spec: &ConnectionEditSpec<'_>) {
    let mut ep = updated_endpoint(
        search.endpoint.as_option(),
        spec.endpoint_url,
        spec.options_json,
    );
    if ep.adapter.trim().is_empty() {
        ep.adapter = "mwmbl".to_owned();
    }
    search.endpoint = Some(ep).into();
    search.max_results = spec
        .list_raw
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&n| (1..=20).contains(&n))
        .unwrap_or(5);
}

pub(super) fn apply_connection_edits(
    conn: &w::Connection,
    spec: &ConnectionEditSpec<'_>,
) -> w::ConnectionInput {
    let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
    match cfg.config.as_mut() {
        Some(w::connection_config::Config::Llm(llm)) => {
            llm.endpoint = Some(updated_endpoint(
                llm.endpoint.as_option(),
                spec.endpoint_url,
                spec.options_json,
            ))
            .into();
            llm.model_ids = split_comma(spec.list_raw);
        }
        Some(w::connection_config::Config::Tts(tts)) => apply_tts_edits(tts, spec),
        Some(w::connection_config::Config::Stt(stt)) => apply_stt_edits(stt, spec),
        Some(w::connection_config::Config::Embedding(emb)) => apply_embedding_edits(emb, spec),
        Some(w::connection_config::Config::Mcp(mcp)) => apply_mcp_edits(mcp, spec),
        Some(w::connection_config::Config::Acp(acp)) => {
            acp.endpoint = Some(updated_endpoint(
                acp.endpoint.as_option(),
                spec.endpoint_url,
                spec.options_json,
            ))
            .into();
            if let Some((cmd, _)) = spec.process {
                acp.command = cmd.to_owned();
            }
            acp.args = spec
                .list_raw
                .split_whitespace()
                .map(str::to_owned)
                .collect();
        }
        Some(w::connection_config::Config::Search(search)) => apply_search_edits(search, spec),
        None => {}
    }

    w::ConnectionInput {
        id: conn.id.clone(),
        name: spec.name.to_owned(),
        enabled: conn.enabled,
        expected_revision: conn.revision,
        kind: conn.kind,
        config: Some(cfg).into(),
        ..Default::default()
    }
}

pub(super) fn build_custom_connection_input(
    kind: w::ConnectionKind,
    name: &str,
    adapter_or_cmd: &str,
    url_or_args: &str,
) -> Option<w::ConnectionInput> {
    let ep = w::ConnectionEndpoint {
        adapter: adapter_or_cmd.to_owned(),
        endpoint: url_or_args.to_owned(),
        options_schema: String::new(),
        options: "{}".into(),
        ..Default::default()
    };
    let config = match kind {
        w::ConnectionKind::Llm | w::ConnectionKind::Unspecified => {
            w::connection_config::Config::Llm(Box::new(w::LlmConnection {
                endpoint: Some(ep).into(),
                ..Default::default()
            }))
        }
        w::ConnectionKind::Tts => w::connection_config::Config::Tts(Box::new(w::TtsConnection {
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
        w::ConnectionKind::Stt => w::connection_config::Config::Stt(Box::new(w::SttConnection {
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
        w::ConnectionKind::Embedding => {
            let mut emb_ep = ep;
            if emb_ep.adapter.trim().is_empty() {
                emb_ep.adapter = "openai".to_owned();
            }
            let sakura = emb_ep.adapter.eq_ignore_ascii_case("sakura")
                || is_sakura_endpoint(&emb_ep.endpoint);
            let (model_id, dimensions) = if sakura {
                ("multilingual-e5-large".to_owned(), 1024)
            } else {
                ("text-embedding-3-small".to_owned(), 1536)
            };
            w::connection_config::Config::Embedding(Box::new(w::EmbeddingConnection {
                endpoint: Some(emb_ep).into(),
                model_id,
                dimensions,
                ..Default::default()
            }))
        }
        w::ConnectionKind::Mcp => return None,
        w::ConnectionKind::Search => {
            let mut search_ep = ep;
            if search_ep.adapter.trim().is_empty() {
                search_ep.adapter = "mwmbl".to_owned();
            }
            w::connection_config::Config::Search(Box::new(w::SearchConnection {
                endpoint: Some(search_ep).into(),
                max_results: 5,
                ..Default::default()
            }))
        }
        w::ConnectionKind::Acp => w::connection_config::Config::Acp(Box::new(w::AcpConnection {
            command: adapter_or_cmd.to_owned(),
            args: url_or_args.split_whitespace().map(str::to_owned).collect(),
            endpoint: Some(ep).into(),
            ..Default::default()
        })),
    };
    Some(w::ConnectionInput {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
        enabled: true,
        expected_revision: 0,
        kind: kind.into(),
        config: Some(w::ConnectionConfig {
            config: Some(config),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::mcp_add::parse_process_args;

    #[test]
    fn editing_a_saved_mcp_connection_replaces_only_the_requested_arguments() {
        let mcp = w::McpConnection {
            command: "npx".into(),
            args: ["-y", "old-package", r"C:\My Tools\日本語", ""]
                .map(str::to_owned)
                .to_vec(),
            allowed_tools: vec!["echo".into()],
            ..Default::default()
        };
        let conn = w::Connection {
            id: "saved-mcp".into(),
            kind: w::ConnectionKind::Mcp.into(),
            revision: 3,
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Mcp(Box::new(mcp.clone()))),
                ..Default::default()
            })
            .into(),
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
        let Some(w::connection_config::Config::Mcp(saved)) = input.config.config.as_ref() else {
            panic!("expected MCP settings");
        };
        assert_eq!(saved.command, "npx");
        assert_eq!(saved.args, ["-y", "new-package", r"C:\My Tools\日本語", ""]);
        assert_eq!(saved.allowed_tools, mcp.allowed_tools);
    }
}
