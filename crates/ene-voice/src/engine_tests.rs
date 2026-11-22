use super::*;
use crate::pcm::wav_wrap;
use ene_domain::settings::{CredentialState, SttConnection, TtsConnection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn select_connection_prefers_enabled_and_select_by_id_rejects_disabled() {
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

    let conns = vec![disabled.clone(), enabled];
    assert_eq!(
        select_connection(&conns, ConnectionKind::Stt).unwrap().id,
        "stt-enabled"
    );
    assert!(matches!(
        select_connection(&[disabled], ConnectionKind::Stt),
        Err(SetupError::Disabled(_))
    ));
    assert!(matches!(
        select_connection(&conns, ConnectionKind::Tts),
        Err(SetupError::Missing)
    ));
    assert!(matches!(
        select_connection_by_id(&conns, ConnectionKind::Stt, "stt-disabled"),
        Err(SetupError::Disabled(_))
    ));
    assert_eq!(
        select_connection_by_id(&conns, ConnectionKind::Stt, "stt-enabled")
            .unwrap()
            .id,
        "stt-enabled"
    );
}

#[tokio::test]
async fn sakura_tts_sends_wav_request_and_decodes_wav_response() {
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

    let base = format!("http://{addr}/v1");
    let conn = Connection {
        id: "sakura-tts".into(),
        name: "Sakura TTS".into(),
        enabled: true,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: base.clone(),
                options_schema: String::new(),
                options: r#"{"speed":1.2}"#.into(),
            },
            voice_ids: vec!["zundamon/normal".into()],
        }),
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let engine = HttpEngine::new(
        conn,
        base,
        Some("uuid:secret".into()),
        ConnectionKind::Tts,
        reqwest::Client::new(),
    )
    .unwrap();
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

    let base = format!("http://{addr}/v1");
    let conn = Connection {
        id: "sakura-vv".into(),
        name: "Sakura VV".into(),
        enabled: true,
        kind: ConnectionKind::Tts,
        config: ConnectionConfig::Tts(TtsConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: base.clone(),
                options_schema: String::new(),
                options: r#"{"protocol":"voicevox","speaker":3}"#.into(),
            },
            voice_ids: vec!["3".into()],
        }),
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let engine = HttpEngine::new(
        conn,
        base,
        Some("uuid:secret".into()),
        ConnectionKind::Tts,
        reqwest::Client::new(),
    )
    .unwrap();
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

    let base = format!("http://{addr}/v1");
    let conn = Connection {
        id: "sakura-stt".into(),
        name: "Sakura STT".into(),
        enabled: true,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: base.clone(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec![],
        }),
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let engine = HttpEngine::new(
        conn,
        base,
        Some("uuid:secret".into()),
        ConnectionKind::Stt,
        reqwest::Client::new(),
    )
    .unwrap();
    let text = engine.transcribe(vec![0u8; 320], 16_000).await.unwrap();
    assert_eq!(text, "こんにちは");

    let req = rx.recv().unwrap();
    let text_req = String::from_utf8_lossy(&req);
    assert!(text_req.starts_with("POST /v1/audio/transcriptions "));
    assert!(text_req.contains("whisper-large-v3-turbo"));
    assert!(text_req.contains("name=\"language\"\r\n\r\nja"));
    assert!(!text_req.contains("name=\"response_format\""));
}
