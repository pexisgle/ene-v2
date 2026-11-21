use crate::{
    choices::ModelRoleSlot,
    state::{Link, Model},
};
use ene_catalog::GOOGLE_AGENT_PLATFORM;
use ene_rpc as w;
use std::collections::BTreeMap;

fn llm_conn(id: &str, adapter: &str, model_ids: Vec<&str>, healthy: bool) -> w::Connection {
    w::Connection {
        id: id.into(),
        name: id.into(),
        enabled: true,
        kind: w::ConnectionKind::Llm.into(),
        credential_state: "registered".into(),
        last_check: Some(w::ConnectionCheck {
            healthy,
            ..Default::default()
        })
        .into(),
        config: Some(w::ConnectionConfig {
            config: Some(w::connection_config::Config::Llm(Box::new(
                w::LlmConnection {
                    endpoint: Some(w::ConnectionEndpoint {
                        adapter: adapter.into(),
                        endpoint: "https://example.invalid/v1".into(),
                        ..Default::default()
                    })
                    .into(),
                    model_ids: model_ids.into_iter().map(str::to_owned).collect(),
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
fn available_models_merges_configured_fetched_and_checked_with_fallback() {
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(w::Snapshot {
        settings: Some(w::Settings {
            connections: vec![
                llm_conn("c-openai", "openai", vec!["gpt-5.4"], true),
                llm_conn("c-empty", "anthropic", vec![], true),
                w::Connection {
                    id: "c-gap".into(),
                    name: "gap".into(),
                    enabled: true,
                    kind: w::ConnectionKind::Llm.into(),
                    last_check: Some(w::ConnectionCheck {
                        healthy: true,
                        capabilities: vec!["verified_model_list".into()],
                        resources: vec!["gemini-3.8-flash".into()],
                        ..Default::default()
                    })
                    .into(),
                    config: Some(w::ConnectionConfig {
                        config: Some(w::connection_config::Config::Llm(Box::new(
                            w::LlmConnection {
                                endpoint: Some(w::ConnectionEndpoint {
                                    adapter: GOOGLE_AGENT_PLATFORM.into(),
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
                },
            ],
            ..Default::default()
        })
        .into(),
        ..Default::default()
    });
    model.connections.models.insert(
        "c-openai".into(),
        vec![w::Model {
            id: "gpt-5.4".into(),
            name: "GPT 5.4".into(),
            ..Default::default()
        }],
    );

    assert_eq!(
        model.available_models_for_connection("c-openai"),
        vec![("gpt-5.4".into(), "GPT 5.4".into())]
    );
    assert_eq!(
        model.available_models_for_connection("c-empty"),
        vec![("claude-sonnet-4-6".into(), "claude-sonnet-4-6".into())]
    );
    assert_eq!(
        model.available_models_for_connection("c-gap"),
        vec![("gemini-3.8-flash".into(), "gemini-3.8-flash".into())]
    );
}

#[test]
fn voice_stt_and_embedding_choices_merge_checks_and_vendor_defaults() {
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(w::Snapshot {
        settings: Some(w::Settings {
            connections: vec![
                w::Connection {
                    id: "tts-oa".into(),
                    name: "OpenAI TTS".into(),
                    enabled: true,
                    kind: w::ConnectionKind::Tts.into(),
                    config: Some(w::ConnectionConfig {
                        config: Some(w::connection_config::Config::Tts(Box::new(
                            w::TtsConnection {
                                endpoint: Some(w::ConnectionEndpoint {
                                    adapter: "openai".into(),
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
                },
                w::Connection {
                    id: "stt-oa".into(),
                    name: "OpenAI STT".into(),
                    enabled: true,
                    kind: w::ConnectionKind::Stt.into(),
                    config: Some(w::ConnectionConfig {
                        config: Some(w::connection_config::Config::Stt(Box::new(
                            w::SttConnection {
                                endpoint: Some(w::ConnectionEndpoint {
                                    adapter: "openai".into(),
                                    options: r#"{"model":"custom-stt"}"#.into(),
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
                },
                w::Connection {
                    id: "emb-oa".into(),
                    name: "OpenAI Emb".into(),
                    enabled: true,
                    kind: w::ConnectionKind::Embedding.into(),
                    config: Some(w::ConnectionConfig {
                        config: Some(w::connection_config::Config::Embedding(Box::new(
                            w::EmbeddingConnection {
                                model_id: "text-embedding-custom".into(),
                                ..Default::default()
                            },
                        ))),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
        .into(),
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
fn auto_assign_default_model_fills_missing_companion_and_task_slots_once() {
    let mut model = Model::new("conn.json".into());
    model.connections.link = Link::Live {
        version: "1".into(),
    };
    model.connections.providers = Some(Vec::new());
    model.snapshot = Some(w::Snapshot {
        active_companion_id: Some("ene".into()),
        companions: vec![w::Companion {
            id: "ene".into(),
            name: "ene".into(),
            revision: 3,
            ..Default::default()
        }],
        settings: Some(w::Settings {
            connections: vec![llm_conn("c1", "openai", vec!["gpt-5.4-mini"], true)],
            ..Default::default()
        })
        .into(),
        ..Default::default()
    });

    let mut seen = BTreeMap::new();
    let (id, rev, req) = model
        .auto_assign_default_model_request(&seen)
        .expect("missing companion/task roles produce patch");
    assert_eq!((id.as_str(), rev), ("ene", 3));
    assert!(req.companion_assignment.as_option().is_some());
    assert!(req.task_assignment.as_option().is_some());

    seen.insert("ene".into(), 3);
    assert!(model.auto_assign_default_model_request(&seen).is_none());
    assert_eq!(ModelRoleSlot::ALL.len(), 7);
}

#[test]
fn enabled_and_all_choice_collectors_distinguish_enabled_connections() {
    let mut llm_off = llm_conn("llm-off", "anthropic", vec!["claude-sonnet-4-6"], true);
    llm_off.enabled = false;
    let mut model = Model::new("conn.json".into());
    model.snapshot = Some(w::Snapshot {
        settings: Some(w::Settings {
            connections: vec![
                llm_conn("llm-on", "openai", vec!["gpt-5.4"], true),
                llm_off,
                w::Connection {
                    id: "tts-sakura".into(),
                    name: "Sakura TTS".into(),
                    enabled: true,
                    kind: w::ConnectionKind::Tts.into(),
                    config: Some(w::ConnectionConfig {
                        config: Some(w::connection_config::Config::Tts(Box::new(
                            w::TtsConnection {
                                endpoint: Some(w::ConnectionEndpoint {
                                    adapter: "sakura".into(),
                                    ..Default::default()
                                })
                                .into(),
                                voice_ids: vec!["zundamon/normal".into()],
                                ..Default::default()
                            },
                        ))),
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                },
                w::Connection {
                    id: "stt-off".into(),
                    name: "Disabled STT".into(),
                    enabled: false,
                    kind: w::ConnectionKind::Stt.into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
        .into(),
        ..Default::default()
    });

    let enabled_models = model.model_choices();
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

    for slot in ModelRoleSlot::ALL {
        assert!(!slot.label().is_empty());
        assert!(!slot.fallback_hint().is_empty());
        assert!(slot.current(&w::ModelAssignments::default()).is_none());
    }
}
