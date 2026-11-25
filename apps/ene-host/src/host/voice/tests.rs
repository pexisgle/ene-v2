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
use ene_voice::Dispatch;
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
                return Err(VoiceHttpError::Engine("mock tts failed".into()));
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

fn test_runtime() -> tokio::runtime::Handle {
    tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
        Box::leak(Box::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        ))
        .handle()
        .clone()
    })
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(Voice::new(test_runtime()));
    world.insert_resource(Engines {
        tts: Some(Arc::new(FakeTts { fail: false })),
        stt: Some(Arc::new(FakeStt)),
    });
    world
}

fn store_with_client() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
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
    let subscription = world.resource::<Voice>().register("c1");
    let mut events = subscription.control;
    let _audio = subscription.receiver;
    world.resource::<Voice>().publish("ene", "c1");
    on_stream_text(&mut world, "ene", "ひとつめ。");
    let first = world.resource::<Voice>().generation_of("ene").unwrap();
    assert_eq!(first, 1);

    flush(
        world.get_resource::<Voice>(),
        "ene",
        w::FlushReason::BargeIn,
    )
    .unwrap();
    match events.try_recv().expect("a flush reaches the client") {
        ene_voice::VoiceOutputEvent::Flush(flush) => {
            assert_eq!(flush.flush_generation_id, first);
            assert_eq!(flush.reason, ene_voice::FlushReason::BargeIn);
        }
        other => panic!("expected a flush, got {other:?}"),
    }
    let stale = ene_voice::VoiceOutputEvent::Chunk(ene_voice::AudioChunk {
        utterance_id: "one".into(),
        generation_id: first,
        ..Default::default()
    });
    assert!(matches!(
        world.resource::<Voice>().deliver_raw("ene", first, stale),
        Dispatch::Stale
    ));

    on_stream_text(&mut world, "ene", "ふたつめ。");
    let second = world.resource::<Voice>().generation_of("ene").unwrap();
    assert!(second > first);
    let current = ene_voice::VoiceOutputEvent::Chunk(ene_voice::AudioChunk {
        utterance_id: "two".into(),
        generation_id: second,
        ..Default::default()
    });
    assert!(matches!(
        world
            .resource::<Voice>()
            .deliver_raw("ene", second, current),
        Dispatch::Sent
    ));
}

#[test]
fn turn_end_releases_the_floor_and_relays_the_utterance() {
    let (_root, store) = store_with_client();
    let mut world = voice_world(&store);
    world.resource::<Voice>().publish("ene", "c1");
    let mut events = world.resource::<Notifier>().subscribe();
    on_stream_text(&mut world, "ene", "こんにちは。");
    let voice = world.get_resource::<Voice>().cloned();
    let mut notifier = world.get_resource_mut::<Notifier>();
    on_turn_end(voice.as_ref(), notifier.as_deref_mut(), "ene");
    assert!(world.resource::<Voice>().floor_holder().is_none());
    world.resource_mut::<Notifier>().flush();
    let notification = events.try_recv().expect("speech_completed is broadcast");
    let Some(w::notification::Event::Generic(event)) = notification.event else {
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
    let world = voice_world(&store);
    let store_res = world.get_resource::<Store>();
    assert!(matches!(
        bind_input(store_res, ""),
        Err(HostError::Invalid(_))
    ));
    assert!(matches!(
        bind_input(store_res, "missing"),
        Err(HostError::NotFound(_))
    ));
    assert!(matches!(
        bind_input(store_res, "c2"),
        Err(HostError::Conflict(_))
    ));
    let binding = bind_input(store_res, "c1").unwrap();
    assert!(binding.accepts("c1"));
    assert!(binding.accepts(""));
    assert!(!binding.accepts("c2"));
}

#[test]
fn voice_connection_prefers_enabled_connection_over_earlier_disabled_one() {
    use crate::host::settings::SttConnection;
    let (_root, store) = store_with_client();
    let rev = store.settings_repo().settings_revision().unwrap();
    let disabled = Connection {
        id: "stt-disabled".into(),
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
        id: "stt-enabled".into(),
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
    let world = voice_world(&store);
    let selected = voice_connection(world.get_resource::<Store>(), ConnectionKind::Stt).unwrap();
    assert_eq!(selected.id, "stt-enabled");
}

#[test]
fn b11_companion_voice_binding_selects_bound_tts_connection_without_silent_fallback() {
    let mut world = World::new();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.db")).unwrap();
    let conn_a = Connection {
        id: "tts-a".into(),
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
        id: "tts-b".into(),
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
    comp.voice = crate::host::json::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-b", "voice_id": "8"}),
    );
    world.insert_resource(store);

    let Err(err) = companion_tts_source(&world, Some(&comp)) else {
        panic!("bound disabled TTS connection must not fall back to another connection");
    };
    assert!(matches!(err, SetupError::Disabled(_)), "{err:?}");

    comp.voice = crate::host::json::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-a", "voice_id": "3", "speed": 1.15}),
    );
    assert!(companion_tts_source(&world, Some(&comp)).is_ok());
}
