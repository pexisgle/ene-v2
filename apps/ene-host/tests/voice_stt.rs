#![cfg(unix)]

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{path::Path, time::Duration};

fn connection_file(runtime: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(runtime.join("connection.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        value.get("endpoint")?.as_str()?.to_owned(),
        value.get("token")?.as_str()?.to_owned(),
    ))
}

fn spawn(state: &Path, runtime: &Path) -> Box<dyn ChildWrapper> {
    CommandWrap::with_new(env!("CARGO_BIN_EXE_ene-host"), |command| {
        command
            .env("ENE_HOST_STATE_DIR", state)
            .env("ENE_HOST_RUNTIME_DIR", runtime)
            .env("ENE_HOST_SESSION_CREDENTIALS", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    })
    .wrap(KillOnDrop)
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

async fn stop(child: &mut Box<dyn ChildWrapper>) {
    child.signal(15).expect("send SIGTERM");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("host did not stop")
        .expect("wait for host");
    assert!(status.success(), "host shutdown failed: {status}");
}

#[tokio::test]
#[ignore = "requires ENE_STT_FIXTURE and OPENAI_API_KEY"]
async fn streamed_audio_is_transcribed() {
    let fixture =
        std::env::var("ENE_STT_FIXTURE").expect("set ENE_STT_FIXTURE to 24 kHz 16-bit mono PCM");
    assert!(
        std::env::var_os("OPENAI_API_KEY").is_some(),
        "set OPENAI_API_KEY"
    );
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
