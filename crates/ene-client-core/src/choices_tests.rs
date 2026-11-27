use crate::{
    GOOGLE_AGENT_PLATFORM,
    domain::{
        Connection, ConnectionCheck, ConnectionConfig, ConnectionEndpoint, ConnectionKind,
        EmbeddingConnection, LlmConnection, ModelAssignments, ModelInfo, ModelRole, Settings,
        Snapshot, SttConnection, TtsConnection,
    },
    state::Model,
};

fn llm_conn(id: &str, adapter: &str, model_ids: Vec<&str>, healthy: bool) -> Connection {
    Connection {
        id: id.into(),
        name: id.into(),
        enabled: true,
        kind: ConnectionKind::Llm,
        credential_state: ene_domain::CredentialState::Registered,
        last_check: Some(ConnectionCheck {
            healthy,
            ..Default::default()
        }),
        config: Some(ConnectionConfig::Llm(LlmConnection {
            endpoint: Some(ConnectionEndpoint {
                adapter: adapter.into(),
                endpoint: "https://example.invalid/v1".into(),
                ..Default::default()
            }),
            model_ids: model_ids.into_iter().map(str::to_owned).collect(),
        })),
        ..Default::default()
    }
}

#[test]
fn available_models_merges_configured_fetched_and_checked_with_fallback() {
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(Snapshot {
        settings: Some(Settings {
            connections: vec![
                llm_conn(
                    "c-openai",
                    "openai",
                    vec![" gpt-5.4 ", "configured", "gpt-5.4"],
                    true,
                ),
                llm_conn("c-empty", "anthropic", vec![], true),
                Connection {
                    id: "c-gap".into(),
                    name: "gap".into(),
                    enabled: true,
                    kind: ConnectionKind::Llm,
                    last_check: Some(ConnectionCheck {
                        healthy: true,
                        capabilities: vec!["verified_model_list".into()],
                        resources: vec!["gemini-3.8-flash".into()],
                        ..Default::default()
                    }),
                    config: Some(ConnectionConfig::Llm(LlmConnection {
                        endpoint: Some(ConnectionEndpoint {
                            adapter: GOOGLE_AGENT_PLATFORM.into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    });
    model.connections.models.insert(
        "c-openai".into(),
        [
            ("gpt-5.4", " "),
            ("discovered", "Discovered"),
            ("gpt-5.4", " GPT 5.4 "),
            ("gpt-5.4", "Another label"),
        ]
        .into_iter()
        .map(|(id, name)| ModelInfo {
            id: id.into(),
            name: name.into(),
        })
        .collect(),
    );

    assert_eq!(
        model.available_models_for_connection("c-openai"),
        vec![
            ("gpt-5.4".into(), "GPT 5.4".into()),
            ("configured".into(), "configured".into()),
            ("discovered".into(), "Discovered".into()),
        ]
    );
    assert_eq!(
        model.available_models_for_connection("c-empty"),
        [
            "claude-sonnet-4-6",
            "claude-3-5-haiku-latest",
            "claude-sonnet-4-20250514",
        ]
        .map(|id| (id.into(), id.into()))
    );
    assert_eq!(
        model.available_models_for_connection("c-gap"),
        vec![("gemini-3.8-flash".into(), "gemini-3.8-flash".into())]
    );
}

#[test]
fn voice_stt_and_embedding_choices_merge_checks_and_vendor_defaults() {
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(Snapshot {
        settings: Some(Settings {
            connections: vec![
                Connection {
                    id: "tts-oa".into(),
                    name: "OpenAI TTS".into(),
                    enabled: true,
                    kind: ConnectionKind::Tts,
                    config: Some(ConnectionConfig::Tts(TtsConnection {
                        endpoint: Some(ConnectionEndpoint {
                            adapter: "openai".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                Connection {
                    id: "stt-oa".into(),
                    name: "OpenAI STT".into(),
                    enabled: true,
                    kind: ConnectionKind::Stt,
                    config: Some(ConnectionConfig::Stt(SttConnection {
                        endpoint: Some(ConnectionEndpoint {
                            adapter: "openai".into(),
                            options: r#"{"model":"custom-stt"}"#.into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                Connection {
                    id: "emb-oa".into(),
                    name: "OpenAI Emb".into(),
                    enabled: true,
                    kind: ConnectionKind::Embedding,
                    config: Some(ConnectionConfig::Embedding(EmbeddingConnection {
                        model_id: "text-embedding-custom".into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    });

    assert!(
        model
            .tts_voice_choices()
            .iter()
            .any(|(cid, vid, _, _)| cid == "tts-oa" && vid == "alloy")
    );
    assert_eq!(
        model.stt_model_choices_for_connection("stt-oa")[0].0,
        "custom-stt"
    );
    assert_eq!(
        model.embedding_model_choices_for_connection("emb-oa")[0].0,
        "text-embedding-custom"
    );
    assert_eq!(
        model.active_stt_selection(),
        Some(("stt-oa".into(), "custom-stt".into()))
    );
}

#[test]
fn enabled_and_all_choice_collectors_distinguish_enabled_connections() {
    let mut llm_off = llm_conn("llm-off", "anthropic", vec!["claude-sonnet-4-6"], true);
    llm_off.enabled = false;
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(Snapshot {
        settings: Some(Settings {
            connections: vec![
                llm_conn("llm-on", "openai", vec!["gpt-5.4"], true),
                llm_off,
                Connection {
                    id: "tts-sakura".into(),
                    name: "Sakura TTS".into(),
                    enabled: true,
                    kind: ConnectionKind::Tts,
                    config: Some(ConnectionConfig::Tts(TtsConnection {
                        endpoint: Some(ConnectionEndpoint {
                            adapter: "sakura".into(),
                            ..Default::default()
                        }),
                        voice_ids: vec!["zundamon/normal".into()],
                    })),
                    ..Default::default()
                },
                Connection {
                    id: "stt-off".into(),
                    name: "Disabled STT".into(),
                    enabled: false,
                    kind: ConnectionKind::Stt,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    });

    let enabled_models = model
        .all_llm_models()
        .into_iter()
        .filter(|(id, _, _)| {
            model
                .settings()
                .unwrap()
                .connections
                .iter()
                .any(|connection| connection.id == *id && connection.enabled)
        })
        .collect::<Vec<_>>();
    assert!(
        enabled_models
            .iter()
            .any(|(cid, mid, _)| cid == "llm-on" && mid == "gpt-5.4")
    );
    assert!(!enabled_models.iter().any(|(cid, _, _)| cid == "llm-off"));

    let all_llm = model.all_llm_models();
    assert!(all_llm.iter().any(|(cid, _, _)| cid == "llm-on"));
    assert!(all_llm.iter().any(|(cid, _, _)| cid == "llm-off"));

    let all_stt = model.all_stt_models();
    assert!(all_stt.iter().any(|(cid, _, _)| cid == "stt-off"));
    assert!(model.active_stt_selection().is_none());

    let all_tts = model.all_tts_voices();
    assert!(
        all_tts
            .iter()
            .any(|(cid, vid, _)| cid == "tts-sakura" && vid == "zundamon/normal")
    );
    assert_eq!(
        model.active_tts_selection(),
        Some(("tts-sakura".into(), "zundamon/normal".into()))
    );

    for slot in ModelRole::ALL {
        assert!(ModelAssignments::default().configured(slot).is_none());
    }
}
