use super::{BigTts, FakeTts, chunks, finished, pcm_bytes, settle, store_with_client, voice_world};
use crate::host::voice::*;
use tokio::sync::Notify;

#[tokio::test]
async fn the_worker_orders_audio_and_finishes_the_utterance() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut events = voice.register("c1").receiver;
    voice.publish("ene", "c1");
    struct DelayedTts(Notify);
    impl TtsEngine for DelayedTts {
        fn synthesize<'a>(
            &'a self,
            text: String,
            _: String,
        ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>> {
            Box::pin(async move {
                if text.starts_with("first") {
                    self.0.notified().await;
                } else {
                    self.0.notify_one();
                }
                let first = text.starts_with("first");
                let rate = if first { 48_000 } else { 44_100 };
                Ok((
                    pcm_bytes((0..128).map(|index| if first { index * 97 } else { -index * 53 })),
                    rate,
                ))
            })
        }
    }
    voice.begin("ene", Arc::new(DelayedTts(Notify::new())), String::new());
    voice.speak("ene", "first。second。");
    voice.end_turn("ene");
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        while collected.len() < 3 {
            collected.push(events.recv().await.unwrap());
        }
        collected
    })
    .await
    .expect("the worker finishes the utterance");
    assert_eq!(chunks(&collected), [0, 1]);
    assert_eq!(finished(&collected), Some(2));
    for event in &collected {
        if let Some(w::audio_output_event::Event::Chunk(chunk)) = &event.event {
            let first = chunk.sequence_no == 0;
            let pcm = pcm_bytes((0..128).map(|index| if first { index * 97 } else { -index * 53 }));
            let rate = if first { 48_000 } else { 44_100 };
            assert_eq!(chunk.sample_rate, SAMPLE_RATE);
            assert_eq!(chunk.pcm_data, wire_pcm(pcm, rate, &mut None).unwrap());
        }
    }
    settle(&voice).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn a_stalled_owner_does_not_block_another() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let slow = voice.register("c1");
    let mut slow_control = slow.control;
    let _slow_audio = slow.receiver; // never drained: the owner is stalled
    let mut fast = voice.register("c2").receiver;
    voice.publish("ene", "c1");
    voice.publish("kuro", "c2");
    voice.begin("ene", Arc::new(BigTts), String::new());
    voice.speak("ene", "slow。");
    voice.end_turn("ene");
    // The stalled owner released the floor at its turn end, so the other
    // companion claims it and finishes.
    voice.begin("kuro", Arc::new(BigTts), String::new());
    voice.speak("kuro", "fast。");
    voice.end_turn("kuro");

    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        while collected.len() < 2 {
            collected.push(fast.recv().await.unwrap());
        }
        collected
    })
    .await
    .expect("a stalled owner does not stop another");
    assert_eq!(chunks(&collected), [0]);
    assert_eq!(finished(&collected), Some(1));

    let flush = tokio::time::timeout(Duration::from_secs(5), slow_control.recv())
        .await
        .expect("the stalled owner is flushed")
        .expect("a flush event");
    assert!(matches!(
        flush.event,
        Some(w::audio_output_event::Event::Flush(flush))
            if flush.reason == w::FlushReason::Cancelled
    ));
    assert!(!voice.lock().owners.contains_key("ene"));
}

#[test]
fn voice_connection_prefers_enabled_connection_over_earlier_disabled_one() {
    use crate::host::settings::{Endpoint, SttConnection};
    let (_root, store) = store_with_client();
    let rev = store.settings_revision().unwrap();
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
        credential_state: crate::host::settings::CredentialState::Absent,
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
        credential_state: crate::host::settings::CredentialState::Absent,
        revision: 0,
        last_check: None,
    };
    store
        .update_settings(rev, &[(disabled, 0), (enabled, 0)])
        .unwrap();
    let world = voice_world(&store);
    let selected = voice_connection(&world, ConnectionKind::Stt).unwrap();
    assert_eq!(selected.id, "stt-enabled");
}

#[tokio::test]
async fn sakura_tts_sends_wav_request_and_decodes_wav_response() {
    use crate::host::settings::{Endpoint, TtsConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let wav = wav_wrap(
        &[100i16.to_le_bytes(), (-100i16).to_le_bytes()].concat(),
        24_000,
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&buf[..pos]);
                let cl = header
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + cl {
                    break;
                }
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            wav.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.write_all(&wav).await;
    });

    let conn = Connection {
        id: "sakura-tts".into(),
        name: "Sakura TTS".into(),
        enabled: true,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: format!("http://{addr}/v1"),
                options_schema: String::new(),
                options: r#"{"speed":1.2}"#.into(),
            },
            voice_ids: vec!["zundamon/normal".into()],
        }),
        credential_state: crate::host::settings::CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let credentials = Credentials::new(true);
    credentials.remember("sakura-tts", "uuid:secret".into());
    let engine = HttpEngine::new(conn, Some(&credentials), None, ConnectionKind::Tts).unwrap();
    let (pcm, rate) = engine
        .synthesize("テストなのだ".into(), "shikokumetan/amaama".into())
        .await
        .unwrap();
    assert_eq!(rate, 24_000);
    assert_eq!(
        pcm,
        [100i16.to_le_bytes(), (-100i16).to_le_bytes()].concat()
    );

    let raw_req = rx.recv().unwrap();
    let (head, body) = raw_req.split_once("\r\n\r\n").unwrap();
    let head_lower = head.to_ascii_lowercase();
    assert!(head.starts_with("POST /v1/audio/speech "));
    assert!(head_lower.contains("authorization: bearer uuid:secret"));
    assert!(head_lower.contains("accept: audio/wav"));
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["model"], "shikokumetan");
    assert_eq!(parsed["voice"], "amaama");
    assert_eq!(parsed["input"], "テストなのだ");
    assert_eq!(parsed["response_format"], "wav");
    assert!(parsed.get("speed").is_none());
}

#[tokio::test]
async fn sakura_tts_voicevox_protocol_uses_tts_v1_paths() {
    use crate::host::settings::{Endpoint, TtsConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let wav = wav_wrap(&[42i16.to_le_bytes()].concat(), 24_000).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        for i in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = tx.send(req);
            if i == 0 {
                let body = r#"{"accent_phrases":[]}"#;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            } else {
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    wav.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.write_all(&wav).await;
            }
        }
    });

    let conn = Connection {
        id: "sakura-vv".into(),
        name: "Sakura VV".into(),
        enabled: true,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: format!("http://{addr}/v1"),
                options_schema: String::new(),
                options: r#"{"protocol":"voicevox","speaker":3}"#.into(),
            },
            voice_ids: vec!["3".into()],
        }),
        credential_state: crate::host::settings::CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let credentials = Credentials::new(true);
    credentials.remember("sakura-vv", "uuid:secret".into());
    let engine = HttpEngine::new(conn, Some(&credentials), None, ConnectionKind::Tts).unwrap();
    let (pcm, rate) = engine
        .synthesize("テスト".into(), "3".into())
        .await
        .unwrap();
    assert_eq!(rate, 24_000);
    assert_eq!(pcm, 42i16.to_le_bytes());
    let first = rx.recv().unwrap();
    let second = rx.recv().unwrap();
    assert!(first.starts_with("POST /tts/v1/audio_query?"));
    assert!(second.starts_with("POST /tts/v1/synthesis?"));
    assert!(
        first
            .to_ascii_lowercase()
            .contains("authorization: bearer uuid:secret")
    );
}

#[tokio::test]
async fn sakura_stt_uses_whisper_turbo_and_omits_response_format() {
    use crate::host::settings::{Endpoint, SttConnection};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&buf[..pos]);
                let cl = header
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + cl {
                    break;
                }
            }
        }
        let _ = tx.send(buf);
        let body = r#"{"model":"whisper-large-v3-turbo","text":"こんにちは"}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(reply.as_bytes()).await;
    });

    let conn = Connection {
        id: "sakura-stt".into(),
        name: "Sakura STT".into(),
        enabled: true,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: format!("http://{addr}/v1"),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec![],
        }),
        credential_state: crate::host::settings::CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let credentials = Credentials::new(true);
    credentials.remember("sakura-stt", "uuid:secret".into());
    let engine = HttpEngine::new(conn, Some(&credentials), None, ConnectionKind::Stt).unwrap();
    let text = engine.transcribe(vec![0u8; 320], 16_000).await.unwrap();
    assert_eq!(text, "こんにちは");

    let req = rx.recv().unwrap();
    let text_req = String::from_utf8_lossy(&req);
    assert!(text_req.starts_with("POST /v1/audio/transcriptions "));
    assert!(text_req.contains("whisper-large-v3-turbo"));
    assert!(text_req.contains("name=\"language\"\r\n\r\nja"));
    assert!(!text_req.contains("name=\"response_format\""));
}

#[test]
fn b11_companion_voice_binding_selects_bound_tts_connection_without_silent_fallback() {
    use crate::host::settings::TtsConnection;
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
        credential_state: crate::host::settings::CredentialState::Absent,
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
        credential_state: crate::host::settings::CredentialState::Absent,
        revision: 1,
        last_check: None,
    };
    store
        .update_settings(1, &[(conn_a, 0), (conn_b, 0)])
        .unwrap();
    let mut comp = crate::host::mind::default_ene_companion("2026-01-01T00:00:00Z");
    store.insert_companion(&comp).unwrap();
    comp.voice = crate::host::json::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-b", "voice_id": "8"}),
    );
    world.insert_resource(store);

    let err = match companion_tts_source(&world, Some(&comp)) {
        Err(err) => err,
        Ok(_) => panic!("bound disabled TTS connection must not fall back to another connection"),
    };
    assert!(matches!(err, SetupError::Disabled(_)), "{err:?}");

    comp.voice = crate::host::json::JsonDoc::new(
        "ene.voice.v1",
        serde_json::json!({"connection_id": "tts-a", "voice_id": "3", "speed": 1.15}),
    );
    assert!(companion_tts_source(&world, Some(&comp)).is_ok());
}

#[tokio::test]
async fn f01_audio_events_carry_owning_companion_id() {
    let voice = Voice::new(tokio::runtime::Handle::current());
    let mut sub = voice.register("desk-1");
    voice.publish("ene", "desk-1");
    voice.publish("miku", "desk-1");

    assert!(voice.begin("miku", Arc::new(FakeTts { fail: false }), "1".into()));
    voice.speak("miku", "こんにちは。");
    let ended = voice.end_turn("miku");
    assert!(ended.is_some());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut chunk_companion = None;
    let mut finished_companion = None;
    while tokio::time::Instant::now() < deadline
        && (chunk_companion.is_none() || finished_companion.is_none())
    {
        if let Ok(event) = sub.receiver.try_recv() {
            match event.event {
                Some(w::audio_output_event::Event::Chunk(chunk)) => {
                    chunk_companion = Some(chunk.companion_id);
                }
                Some(w::audio_output_event::Event::Finished(fin)) => {
                    finished_companion = Some(fin.companion_id);
                }
                _ => {}
            }
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(chunk_companion.as_deref(), Some("miku"));
    assert_eq!(finished_companion.as_deref(), Some("miku"));
}
