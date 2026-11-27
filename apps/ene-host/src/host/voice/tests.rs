use super::engine::voice_connection;
use super::*;
use crate::host::{
    db::{Store, clients::ClientDeclaration},
    error::HostError,
    notify::Notifier,
    settings::{
        Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, TtsConnection,
    },
};
use bevy_ecs::prelude::World;
use ene_rpc as w;
use std::sync::Arc;

struct FakeTts {
    fail: bool,
}

impl TtsEngine for FakeTts {
    fn synthesize<'a>(
        &'a self,
        text: String,
        _: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
        Box::pin(async move {
            if self.fail {
                return Err(VoiceHttpError::Transport("mock tts failed".into()));
            }
            Ok((
                text.bytes()
                    .flat_map(|byte| i16::from(byte).to_le_bytes())
                    .collect(),
                SAMPLE_RATE,
            ))
        })
    }
}

struct FakeStt;

impl SttEngine for FakeStt {
    fn transcribe<'a>(&'a self, pcm: Vec<u8>, _: u32) -> BoxFuture<'a, HttpResult<String>> {
        Box::pin(async move { Ok(format!("{} bytes", pcm.len())) })
    }
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(Voice::new(tokio::runtime::Handle::current()));
    world.insert_resource(Engines {
        tts: Some(Arc::new(FakeTts { fail: false })),
        stt: Some(Arc::new(FakeStt)),
    });
    world
}

fn store_with_client() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    store.companion_repo().seed_initial_companion().unwrap();
    store
        .client_repo()
        .register_client(&ClientDeclaration {
            id: "c1",
            name: "desk",
            platform: "linux",
            capabilities: &[],
            specs: &[],
            expected_revision: 0,
        })
        .unwrap();
    store.client_repo().activate("ene", Some("c1")).unwrap();
    (root, store)
}

fn voice_world(store: &Store) -> World {
    let mut world = test_world();
    world.insert_resource(store.clone());
    world.insert_resource(Notifier::new());
    world
}

#[tokio::test]
async fn a_flush_invalidates_the_older_generation() {
    let mut world = test_world();
    let mut subscription = world.resource::<Voice>().register("c1");
    world.resource::<Voice>().publish("ene", "c1");
    on_stream_text(&mut world, "ene", "ひとつめ。");
    flush(
        world.get_resource::<Voice>(),
        "ene",
        w::FlushReason::BargeIn,
    )
    .unwrap();
    let ene_voice::VoiceOutputEvent::Flush(flush) = subscription
        .control
        .try_recv()
        .expect("a flush reaches the client")
    else {
        panic!("expected a flush event");
    };
    assert_eq!(flush.reason, ene_voice::FlushReason::BargeIn);

    on_stream_text(&mut world, "ene", "ふたつめ。");
    let current = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        subscription.receiver.recv(),
    )
    .await
    .unwrap()
    .unwrap();
    let ene_voice::VoiceOutputEvent::Chunk(chunk) = current else {
        panic!("expected audio from the new generation");
    };
    assert!(chunk.generation_id > flush.flush_generation_id);
    let expected: Vec<_> = "ふたつめ。"
        .bytes()
        .flat_map(|byte| i16::from(byte).to_le_bytes())
        .collect();
    assert_eq!(chunk.pcm_data, expected);
}

#[tokio::test]
async fn a_peer_holding_the_floor_stops_the_waiting_companions_turn() {
    use bevy_ecs::prelude::On;
    use rig_ecs::prelude::StopTurn;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut world = test_world();
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&stopped);
    world
        .spawn(crate::host::resources::CompanionAgent {
            companion_id: "ene".into(),
        })
        .observe(move |_: On<StopTurn>| observed.store(true, Ordering::Relaxed));
    let voice = world.resource::<Voice>();
    voice.publish("ene", "c1");
    voice.publish("peer", "c1");
    assert!(voice.begin("peer", Arc::new(FakeTts { fail: false }), String::new()));

    on_stream_text(&mut world, "ene", "こんにちは。");

    let voice = world.resource::<Voice>();
    assert!(stopped.load(Ordering::Relaxed));
    assert_eq!(voice.floor_holder().as_deref(), Some("peer"));
    assert!(!voice.streaming("ene"));
}

#[tokio::test]
async fn turn_end_releases_the_floor_and_relays_the_utterance() {
    let (_root, store) = store_with_client();
    let mut world = voice_world(&store);
    world.resource::<Voice>().publish("ene", "c1");
    let mut events = world.resource::<Notifier>().subscribe();
    on_stream_text(&mut world, "ene", "こんにちは。");
    on_turn_end(
        world.get_resource::<Voice>(),
        world.get_resource::<Notifier>(),
        "ene",
    );
    assert!(world.resource::<Voice>().floor_holder().is_none());
    world.resource_mut::<Notifier>().flush();
    let notification = events.try_recv().expect("speech_completed is broadcast");
    let Some(w::notification::Event::Generic(event)) = notification.event.as_ref() else {
        panic!("expected a generic host event");
    };
    assert_eq!(event.kind, "speech_completed");
    let payload = event
        .payload
        .as_option()
        .expect("the event names the utterance");
    let payload: serde_json::Value = serde_json::from_str(&payload.json).unwrap();
    assert_eq!(payload["companion_id"], "ene");
    assert_eq!(payload["client_id"], "c1");
    assert_eq!(payload["total_sentences"], 1);
    assert!(
        payload["utterance_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(
        payload["generation_id"]
            .as_u64()
            .is_some_and(|generation| generation > 0)
    );
}

#[test]
fn input_binding_validates_first_metadata_and_allows_omitted_followups() {
    let (_root, store) = store_with_client();
    store
        .client_repo()
        .register_client(&ClientDeclaration {
            id: "c2",
            name: "handheld",
            platform: "android",
            capabilities: &[],
            specs: &[],
            expected_revision: 0,
        })
        .unwrap();
    let lookup = |id| active_companions(&store, &ene_domain::ClientId::parse(id).unwrap());
    assert!(matches!(lookup("missing"), Err(HostError::NotFound(_))));
    assert!(matches!(lookup("c2"), Err(HostError::Conflict(_))));
    assert_eq!(lookup("c1").unwrap(), ["ene"]);
}

#[test]
fn voice_connection_prefers_enabled_connection_over_earlier_disabled_one() {
    use crate::host::settings::SttConnection;
    let (_root, store) = store_with_client();
    let rev = store.settings_repo().settings_revision().unwrap();
    let disabled = Connection {
        id: ene_domain::ConnectionId::parse("stt-disabled").unwrap(),
        name: "Disabled STT".into(),
        enabled: false,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "openai".into(),
                endpoint: "http://127.0.0.1:8080/v1".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec!["ja".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    };
    let enabled = Connection {
        id: ene_domain::ConnectionId::parse("stt-enabled").unwrap(),
        name: "Enabled STT".into(),
        enabled: true,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "openai".into(),
                endpoint: "http://127.0.0.1:8081/v1".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec!["ja".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    };
    store
        .settings_repo()
        .update_settings(rev, &[(disabled, 0), (enabled, 0)])
        .unwrap();
    let selected = voice_connection(Some(&store), ConnectionKind::Stt).unwrap();
    assert_eq!(selected.id.as_str(), "stt-enabled");
}

#[test]
fn b11_companion_voice_binding_selects_bound_tts_connection_without_silent_fallback() {
    let mut world = World::new();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.db")).unwrap();
    let conn_a = Connection {
        id: ene_domain::ConnectionId::parse("tts-a").unwrap(),
        name: "Enabled TTS".into(),
        enabled: true,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "voicevox".into(),
                endpoint: "http://127.0.0.1:50021".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            voice_ids: vec!["3".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    let conn_b = Connection {
        id: ene_domain::ConnectionId::parse("tts-b").unwrap(),
        name: "Disabled TTS".into(),
        enabled: false,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "voicevox".into(),
                endpoint: "http://127.0.0.1:50022".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            voice_ids: vec!["8".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    store
        .settings_repo()
        .update_settings(1, &[(conn_a, 0), (conn_b, 0)])
        .unwrap();
    let mut comp = crate::host::mind::default_ene_companion("2026-01-01T00:00:00Z");
    store.companion_repo().insert_companion(&comp).unwrap();
    comp.voice = ene_mind::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-b", "voice_id": "8"}),
    );
    world.insert_resource(store);

    let Err(err) = tts_source(
        &VoiceEngineContext::for_test(&world),
        Some(&parse_binding(&comp)),
    ) else {
        panic!("bound disabled TTS connection must not fall back to another connection");
    };
    assert!(matches!(err, SetupError::Disabled(_)), "{err:?}");

    comp.voice = ene_mind::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-a", "voice_id": "3", "speed": 1.15}),
    );
    assert!(
        tts_source(
            &VoiceEngineContext::for_test(&world),
            Some(&parse_binding(&comp))
        )
        .is_ok()
    );
}
