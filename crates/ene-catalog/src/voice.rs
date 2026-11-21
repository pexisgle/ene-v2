//! Voice (STT/TTS) connection configuration helpers and resource name parsing.
use crate::vendors::is_sakura_conn;
use ene_rpc as w;

pub const OPENAI_TTS_VOICES: &[&str] = &[
    "alloy", "ash", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer",
];

pub fn parse_voice_resource(raw: &str) -> (String, String) {
    let trimmed = raw.trim();
    if let Some((id, name)) = trimmed.split_once(':') {
        let id = id.trim();
        let name = name.trim();
        if !id.is_empty() && !name.is_empty() {
            return (id.to_owned(), name.to_owned());
        }
    }
    (trimmed.to_owned(), trimmed.to_owned())
}

pub fn apply_tts_voice_to_config(
    cfg: &mut w::ConnectionConfig,
    is_sakura: bool,
    voice_clean: &str,
) {
    if let Some(w::connection_config::Config::Tts(tts)) = cfg.config.as_mut() {
        tts.voice_ids = vec![voice_clean.to_owned()];
        let mut ep = tts.endpoint.as_option().cloned().unwrap_or_default();
        let mut opts = match serde_json::from_str::<serde_json::Value>(&ep.options) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        if is_sakura {
            if let Some((m, v)) = voice_clean.split_once('/') {
                if !m.trim().is_empty() {
                    opts.insert("model".into(), serde_json::json!(m.trim()));
                }
                if !v.trim().is_empty() {
                    opts.insert("voice".into(), serde_json::json!(v.trim()));
                }
            } else {
                opts.insert("model".into(), serde_json::json!(voice_clean));
            }
            opts.entry("protocol".to_owned())
                .or_insert_with(|| serde_json::json!("sakura"));
            opts.entry("response_format".to_owned())
                .or_insert_with(|| serde_json::json!("wav"));
            ep.options = serde_json::Value::Object(opts).to_string();
        } else if let Ok(speaker_num) = voice_clean.parse::<u32>() {
            opts.insert("speaker".into(), serde_json::json!(speaker_num));
            ep.options = serde_json::Value::Object(opts).to_string();
        } else {
            opts.insert("voice".into(), serde_json::json!(voice_clean));
            ep.options = serde_json::Value::Object(opts).to_string();
        }
        tts.endpoint = Some(ep).into();
    }
}

pub fn apply_stt_model_to_config(cfg: &mut w::ConnectionConfig, model_clean: &str) {
    if let Some(w::connection_config::Config::Stt(stt)) = cfg.config.as_mut() {
        let mut ep = stt.endpoint.as_option().cloned().unwrap_or_default();
        let mut opts = match serde_json::from_str::<serde_json::Value>(&ep.options) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        opts.insert("model".into(), serde_json::json!(model_clean));
        ep.options = serde_json::Value::Object(opts).to_string();
        stt.endpoint = Some(ep).into();
    }
}

pub fn is_local_voicevox_conn(conn: &w::Connection) -> bool {
    conn.kind == w::ConnectionKind::Tts
        && !is_sakura_conn(conn)
        && conn
            .config
            .as_option()
            .and_then(|c| c.config.as_ref())
            .is_some_and(|c| match c {
                w::connection_config::Config::Tts(tts) => tts
                    .endpoint
                    .as_option()
                    .is_some_and(|ep| ep.adapter.eq_ignore_ascii_case("voicevox")),
                _ => false,
            })
}

pub fn stt_model_of_connection(conn: &w::Connection) -> String {
    if let Some(w::connection_config::Config::Stt(stt)) =
        conn.config.as_option().and_then(|c| c.config.as_ref())
        && let Some(ep) = stt.endpoint.as_option()
        && let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options)
        && let Some(model) = opts
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return model.to_owned();
    }
    if is_sakura_conn(conn) {
        "whisper-large-v3-turbo".to_owned()
    } else {
        "whisper-1".to_owned()
    }
}

pub fn tts_voice_of_connection(conn: &w::Connection) -> Option<String> {
    let Some(w::connection_config::Config::Tts(tts)) =
        conn.config.as_option().and_then(|c| c.config.as_ref())
    else {
        return None;
    };
    if let Some(first) = tts
        .voice_ids
        .first()
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (vid, _) = parse_voice_resource(first);
        if !vid.is_empty() {
            return Some(vid);
        }
    }
    let sakura = is_sakura_conn(conn);
    if let Some(ep) = tts.endpoint.as_option() {
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
        let adapter = ep.adapter.trim().to_ascii_lowercase();
        if adapter == "voicevox" {
            return Some("3".to_owned());
        }
        if adapter == "openai" {
            return Some("alloy".to_owned());
        }
    }
    None
}

pub fn active_stt_in_connections(connections: &[w::Connection]) -> Option<(String, String)> {
    connections
        .iter()
        .find(|c| c.kind == w::ConnectionKind::Stt && c.enabled)
        .map(|c| (c.id.clone(), stt_model_of_connection(c)))
}

pub fn active_tts_in_connections(connections: &[w::Connection]) -> Option<(String, String)> {
    connections
        .iter()
        .find(|c| c.kind == w::ConnectionKind::Tts && c.enabled)
        .and_then(|c| tts_voice_of_connection(c).map(|v| (c.id.clone(), v)))
}

pub fn disable_connection_input(conn: &w::Connection) -> w::ConnectionInput {
    w::ConnectionInput {
        id: conn.id.clone(),
        name: conn.name.clone(),
        enabled: false,
        expected_revision: conn.revision,
        kind: conn.kind,
        config: conn.config.clone(),
        ..Default::default()
    }
}

pub fn build_voice_assignment_inputs<F>(
    connections: &[w::Connection],
    kind: w::ConnectionKind,
    selection: Option<(&str, &str)>,
    apply_cfg: F,
) -> Vec<w::ConnectionInput>
where
    F: Fn(&w::Connection, &mut w::ConnectionConfig, &str),
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
                    let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
                    apply_cfg(conn, &mut cfg, clean);
                    inputs.push(w::ConnectionInput {
                        id: conn.id.clone(),
                        name: conn.name.clone(),
                        enabled: true,
                        expected_revision: conn.revision,
                        kind: conn.kind,
                        config: Some(cfg).into(),
                        ..Default::default()
                    });
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

pub fn build_stt_assignment_inputs(
    connections: &[w::Connection],
    selection: Option<(&str, &str)>,
) -> Vec<w::ConnectionInput> {
    build_voice_assignment_inputs(
        connections,
        w::ConnectionKind::Stt,
        selection,
        |_, cfg, m| {
            apply_stt_model_to_config(cfg, m);
        },
    )
}

pub fn build_tts_assignment_inputs(
    connections: &[w::Connection],
    selection: Option<(&str, &str)>,
) -> Vec<w::ConnectionInput> {
    build_voice_assignment_inputs(
        connections,
        w::ConnectionKind::Tts,
        selection,
        |conn, cfg, v| apply_tts_voice_to_config(cfg, is_sakura_conn(conn), v),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendors::SAKURA_AI_ENDPOINT;

    fn make_stt_conn(
        id: &str,
        enabled: bool,
        adapter: &str,
        endpoint: &str,
        model: &str,
    ) -> w::Connection {
        w::Connection {
            id: id.into(),
            name: id.into(),
            enabled,
            revision: 3,
            kind: w::ConnectionKind::Stt.into(),
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Stt(Box::new(
                    w::SttConnection {
                        endpoint: Some(w::ConnectionEndpoint {
                            adapter: adapter.into(),
                            endpoint: endpoint.into(),
                            options: serde_json::json!({ "model": model }).to_string(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
    }

    fn make_tts_conn(
        id: &str,
        enabled: bool,
        adapter: &str,
        endpoint: &str,
        voices: Vec<&str>,
        options: serde_json::Value,
    ) -> w::Connection {
        w::Connection {
            id: id.into(),
            name: id.into(),
            enabled,
            revision: 5,
            kind: w::ConnectionKind::Tts.into(),
            config: Some(w::ConnectionConfig {
                config: Some(w::connection_config::Config::Tts(Box::new(
                    w::TtsConnection {
                        endpoint: Some(w::ConnectionEndpoint {
                            adapter: adapter.into(),
                            endpoint: endpoint.into(),
                            options: options.to_string(),
                            ..Default::default()
                        })
                        .into(),
                        voice_ids: voices.into_iter().map(str::to_owned).collect(),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_voice_resource_extracts_id_and_japanese_name() {
        assert_eq!(
            parse_voice_resource("3:ずんだもん（ノーマル）"),
            ("3".into(), "ずんだもん（ノーマル）".into())
        );
        assert_eq!(
            parse_voice_resource("zundamon/normal:VOICEVOX:ずんだもん（ノーマル）"),
            (
                "zundamon/normal".into(),
                "VOICEVOX:ずんだもん（ノーマル）".into()
            )
        );
        assert_eq!(
            parse_voice_resource("alloy"),
            ("alloy".into(), "alloy".into())
        );
    }

    #[test]
    fn active_stt_and_tts_selection_reads_first_enabled_connection() {
        let connections = vec![
            make_stt_conn(
                "stt-disabled",
                false,
                "openai",
                "https://api.openai.com/v1",
                "whisper-1",
            ),
            make_stt_conn(
                "stt-sakura",
                true,
                "sakura",
                SAKURA_AI_ENDPOINT,
                "whisper-large-v3-turbo",
            ),
            make_tts_conn(
                "tts-voicevox",
                false,
                "voicevox",
                "http://127.0.0.1:50021",
                vec![],
                serde_json::json!({ "speaker": 3 }),
            ),
            make_tts_conn(
                "tts-sakura",
                true,
                "sakura",
                SAKURA_AI_ENDPOINT,
                vec!["zundamon/amaama"],
                serde_json::json!({ "protocol": "sakura", "model": "zundamon", "voice": "amaama" }),
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
        let connections = vec![
            make_stt_conn(
                "stt-openai",
                true,
                "openai",
                "https://api.openai.com/v1",
                "whisper-1",
            ),
            make_stt_conn(
                "stt-sakura",
                false,
                "sakura",
                SAKURA_AI_ENDPOINT,
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
        let Some(w::connection_config::Config::Stt(stt)) =
            activated.config.as_option().and_then(|c| c.config.as_ref())
        else {
            panic!("expected Stt config");
        };
        let opts: serde_json::Value =
            serde_json::from_str(&stt.endpoint.as_option().expect("endpoint").options)
                .expect("json options");
        assert_eq!(opts["model"], "gpt-4o-transcribe");

        let off_inputs = build_stt_assignment_inputs(&connections, None);
        assert_eq!(off_inputs.len(), 1);
        assert_eq!(off_inputs[0].id, "stt-openai");
        assert!(!off_inputs[0].enabled);
    }

    #[test]
    fn tts_assignment_inputs_activates_target_updates_voice_and_disables_others() {
        let connections = vec![
            make_tts_conn(
                "tts-voicevox",
                true,
                "voicevox",
                "http://127.0.0.1:50021",
                vec!["3"],
                serde_json::json!({ "speaker": 3 }),
            ),
            make_tts_conn(
                "tts-sakura",
                false,
                "sakura",
                SAKURA_AI_ENDPOINT,
                vec!["zundamon/normal"],
                serde_json::json!({ "protocol": "sakura", "model": "zundamon", "voice": "normal" }),
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
        let Some(w::connection_config::Config::Tts(tts)) =
            activated.config.as_option().and_then(|c| c.config.as_ref())
        else {
            panic!("expected Tts config");
        };
        assert_eq!(tts.voice_ids, vec!["shikokumetan/amaama"]);
        let opts: serde_json::Value =
            serde_json::from_str(&tts.endpoint.as_option().expect("endpoint").options)
                .expect("json options");
        assert_eq!(opts["model"], "shikokumetan");
        assert_eq!(opts["voice"], "amaama");
        assert_eq!(opts["protocol"], "sakura");
    }
}
