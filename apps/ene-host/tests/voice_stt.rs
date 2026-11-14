//! The microphone path: PCM from a real recording is transcribed by the
//! configured STT connection.
//!
//! Opt-in, because it needs a provider key and a fixture: set
//! `ENE_STT_FIXTURE` to a 24 kHz / 16-bit mono PCM file and `OPENAI_API_KEY`,
//! then `cargo test -p ene-host --test voice_stt -- --nocapture`. Without them
//! the test reports itself as skipped instead of failing CI.
#![cfg(unix)]

use std::{path::Path, process::Command, time::Duration};

fn connection_file(runtime: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(runtime.join("connection.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        value.get("endpoint")?.as_str()?.to_owned(),
        value.get("token")?.as_str()?.to_owned(),
    ))
}

fn spawn(state: &Path, runtime: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_ene-host"))
        .env("ENE_HOST_STATE_DIR", state)
        .env("ENE_HOST_RUNTIME_DIR", runtime)
        .env("ENE_HOST_SESSION_CREDENTIALS", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start ene-host")
}

async fn discovery(runtime: &Path) -> (String, String) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(discovery) = connection_file(runtime) {
                return discovery;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("host did not publish discovery")
}

async fn stop(child: &mut std::process::Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn streamed_audio_is_transcribed() {
    let Ok(fixture) = std::env::var("ENE_STT_FIXTURE") else {
        eprintln!("skipping: set ENE_STT_FIXTURE to 24 kHz 16-bit mono PCM");
        return;
    };
    if std::env::var_os("OPENAI_API_KEY").is_none() {
        eprintln!("skipping: OPENAI_API_KEY is not set");
        return;
    }
    let pcm = std::fs::read(&fixture).expect("the fixture");

    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let mut host = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();

    let host_client = ene_rpc::HostServiceClient::new(transport.clone(), config.clone());
    let settings = ene_rpc::SettingsServiceClient::new(transport.clone(), config.clone());
    let clients = ene_rpc::ClientServiceClient::new(transport.clone(), config.clone());
    settings
        .update_settings(ene_rpc::UpdateSettingsRequest {
            expected_revision: 1,
            connections: vec![ene_rpc::ConnectionInput {
                id: "openai-stt".into(),
                name: "openai-stt".into(),
                enabled: true,
                expected_revision: 0,
                kind: ene_rpc::ConnectionKind::CONNECTION_KIND_STT.into(),
                config: Some(ene_rpc::ConnectionConfig {
                    config: Some(ene_rpc::connection_config::Config::Stt(Box::new(
                        ene_rpc::SttConnection {
                            endpoint: Some(ene_rpc::ConnectionEndpoint {
                                adapter: "openai".into(),
                                endpoint: "https://api.openai.com/v1".into(),
                                ..Default::default()
                            })
                            .into(),
                            language_hints: vec!["en".into()],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .expect("the STT connection is stored");
    let companion = host_client
        .create_companion(ene_rpc::CreateCompanionRequest {
            name: "Mio".into(),
            mind: "kind".into(),
            ..Default::default()
        })
        .await
        .expect("a companion")
        .view()
        .id
        .to_owned();
    clients
        .register_client(ene_rpc::RegisterClientRequest {
            id: "c1".into(),
            name: "desk".into(),
            platform: "linux".into(),
            ..Default::default()
        })
        .await
        .expect("a client");
    let activation = clients
        .activate(ene_rpc::ActivationRequest {
            companion_id: companion,
            client_id: "c1".into(),
            ..Default::default()
        })
        .await
        .expect("the client becomes active")
        .view()
        .revision;

    let voice = ene_rpc::VoiceServiceClient::new(transport, config);
    let mut call = voice
        .stream_audio_input()
        .await
        .expect("the microphone stream opens");
    // 200 ms of audio per frame, at the host's own sample rate.
    for frame in pcm.chunks(9_600) {
        call.send(ene_rpc::AudioInputChunk {
            client_id: "c1".into(),
            pcm_data: frame.to_vec(),
            sample_rate: 24_000,
            activation_revision: activation,
            ..Default::default()
        })
        .await
        .expect("an audio frame");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    call.close_send();

    let mut transcript = String::new();
    for _ in 0..120 {
        let Ok(Ok(Some(message))) = tokio::time::timeout(
            Duration::from_secs(5),
            call.message::<ene_rpc::TranscriptionEvent>(),
        )
        .await
        else {
            continue;
        };
        let event = message.view();
        if event.is_final && !event.text.trim().is_empty() {
            transcript = event.text.to_owned();
            break;
        }
    }
    stop(&mut host).await;
    assert!(
        !transcript.trim().is_empty(),
        "the recording is transcribed; the stream yielded nothing"
    );
    println!("transcript: {transcript}");
}
