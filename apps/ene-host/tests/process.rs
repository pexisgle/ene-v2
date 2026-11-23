//! A real host process: private discovery, owner auth and graceful shutdown.

// Integration test helpers outside `#[test]` functions do not fall under
// clippy's `allow-unwrap-in-tests` heuristic; remove when helpers propagate Results.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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

#[cfg(unix)]
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

#[cfg(unix)]
async fn stop(child: &mut Box<dyn ChildWrapper>) {
    child.signal(15).expect("send SIGTERM");
    wait_for_exit(child).await;
}

async fn wait_for_exit(child: &mut Box<dyn ChildWrapper>) {
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("host did not stop")
        .expect("wait for host");
    assert!(status.success(), "host shutdown failed: {status}");
}

/// Installs an STT provider under the directory the host reads at startup.
/// The host refuses a state directory, and the loader a package, that anyone
/// else may write, so the fixture makes both private, as the host does for a
/// state directory it creates itself.
#[cfg(unix)]
fn install_voice_stt(state: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm");
    assert!(
        wasm.is_file(),
        "build the guest first: cargo build --release --target wasm32-unknown-unknown -p ene-reference-wasm ({})",
        wasm.display()
    );
    let package = state.join("plugins").join("voice-stt");
    std::fs::create_dir_all(&package).unwrap();
    for private in [state.to_path_buf(), package.clone()] {
        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(package.join("main.wasm"), std::fs::read(&wasm).unwrap()).unwrap();
    std::fs::write(
        package.join("manifest.json"),
        serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "voice-stt", "name": "Voice STT fixture", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": ["stt"], "uses": ["http"],
                "provider": {
                    "id": "voice-stt", "endpoint": "https://plugin.example",
                    "auth": {"type": "none"},
                    "destinations": ["https://plugin.example"]
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    for file in ["main.wasm", "manifest.json"] {
        std::fs::set_permissions(package.join(file), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
}

#[tokio::test]
async fn process_stops_when_the_desktop_closes_its_pipe() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let mut child = CommandWrap::with_new(env!("CARGO_BIN_EXE_ene-host"), |command| {
        command
            .env("ENE_HOST_STATE_DIR", temp.path().join("state"))
            .env("ENE_HOST_RUNTIME_DIR", &runtime)
            .env("ENE_HOST_EXIT_ON_STDIN_EOF", "1")
            .stdin(std::process::Stdio::piped());
    })
    .wrap(KillOnDrop)
    .spawn()
    .expect("start the desktop's ene-host");

    discovery(&runtime).await;
    drop(child.stdin().take().expect("host stdin"));
    wait_for_exit(&mut child).await;
    assert!(!runtime.join("connection.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn process_publishes_private_discovery_authenticates_and_stops() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    assert!(runtime.join("connection.json").exists());
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let client = ene_rpc::HostServiceClient::new(transport, config);
    let snapshot = client
        .get_snapshot(ene_rpc::Empty::default())
        .await
        .unwrap();
    assert!(!snapshot.view().server_version.is_empty());
    let (transport, config) = ene_rpc::connect(&endpoint, "wrong-token").unwrap();
    let wrong = ene_rpc::HostServiceClient::new(transport, config);
    assert!(wrong.get_snapshot(ene_rpc::Empty::default()).await.is_err());
    stop(&mut child).await;
    assert!(!runtime.join("connection.json").exists());
}

/// Stores an unreachable connection and its session credential, and creates
/// a companion bound to it. Returns the companion id.
#[cfg(unix)]
async fn seed_companion(
    settings: &ene_rpc::SettingsServiceClient<connectrpc::client::HttpClient>,
    host: &ene_rpc::HostServiceClient<connectrpc::client::HttpClient>,
    assignment: &ene_rpc::ModelAssignment,
) -> String {
    // A connection the turn cannot reach: acceptance and the saved input do
    // not depend on the provider answering.
    settings
        .update_settings(ene_rpc::UpdateSettingsRequest {
            expected_revision: 1,
            connections: vec![ene_rpc::ConnectionInput {
                id: "c1".into(),
                name: "unreachable".into(),
                enabled: true,
                kind: ene_rpc::ConnectionKind::CONNECTION_KIND_LLM.into(),
                config: Some(ene_rpc::ConnectionConfig {
                    config: Some(ene_rpc::connection_config::Config::Llm(Box::new(
                        ene_rpc::LlmConnection {
                            endpoint: Some(ene_rpc::ConnectionEndpoint {
                                adapter: "openai".into(),
                                endpoint: "http://127.0.0.1:1/v1".into(),
                                ..Default::default()
                            })
                            .into(),
                            model_ids: vec!["gpt-4o-mini".into()],
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
        .expect("the connection is stored");
    register_credential(settings).await;
    host.create_companion(ene_rpc::CreateCompanionRequest {
        name: "Mio".into(),
        mind: "kind".into(),
        assignments: Some(ene_rpc::ModelAssignments {
            companion: Some(assignment.clone()).into(),
            task: Some(assignment.clone()).into(),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    })
    .await
    .expect("a companion")
    .view()
    .id
    .to_owned()
}

#[cfg(unix)]
async fn register_credential(
    settings: &ene_rpc::SettingsServiceClient<connectrpc::client::HttpClient>,
) {
    // The credential is session-only, so it is gone after a restart.
    settings
        .register_credentials(ene_rpc::CredentialRequest {
            id: "c1".into(),
            secret: Some("dummy-key".into()),
            ..Default::default()
        })
        .await
        .expect("the credential is stored for this session");
}

#[cfg(unix)]
async fn accept_message(
    chat: &ene_rpc::ChatServiceClient<connectrpc::client::HttpClient>,
    companion: &str,
) -> String {
    chat.send_message(ene_rpc::SendMessageRequest {
        companion_id: companion.to_owned(),
        request_key: "k1".into(),
        text: "覚えていて".into(),
        ..Default::default()
    })
    .await
    .expect("the message is accepted")
    .view()
    .request_id
    .to_owned()
}

#[cfg(unix)]
async fn saved_texts(
    chat: &ene_rpc::ChatServiceClient<connectrpc::client::HttpClient>,
    companion: &str,
) -> Vec<String> {
    chat.get_history(ene_rpc::HistoryRequest {
        companion_id: companion.to_owned(),
        ..Default::default()
    })
    .await
    .expect("the history is readable")
    .view()
    .items
    .iter()
    .map(|item| item.text.to_owned())
    .collect()
}

/// A conversation and its acceptance receipt survive a restart: the restored
/// agent keeps its role, and a retry with the same request key is not run
/// again.
#[cfg(unix)]
#[tokio::test]
async fn process_restores_a_conversation_and_its_acceptance() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let assignment = ene_rpc::ModelAssignment {
        connection_id: "c1".into(),
        model_id: "gpt-4o-mini".into(),
        ..Default::default()
    };

    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let settings = ene_rpc::SettingsServiceClient::new(transport.clone(), config.clone());
    let host = ene_rpc::HostServiceClient::new(transport.clone(), config.clone());
    let chat = ene_rpc::ChatServiceClient::new(transport, config);
    let companion = seed_companion(&settings, &host, &assignment).await;
    let accepted = accept_message(&chat, &companion).await;
    stop(&mut child).await;

    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let settings = ene_rpc::SettingsServiceClient::new(transport.clone(), config.clone());
    let chat = ene_rpc::ChatServiceClient::new(transport, config);
    register_credential(&settings).await;
    assert_eq!(
        saved_texts(&chat, &companion).await,
        ["覚えていて"],
        "the accepted input survives the restart"
    );
    assert_eq!(
        accept_message(&chat, &companion).await,
        accepted,
        "the retry binds to the restored acceptance instead of running again"
    );
    let history = saved_texts(&chat, &companion)
        .await
        .iter()
        .filter(|text| text.as_str() == "覚えていて")
        .count();
    assert_eq!(history, 1, "the retry does not add a second input");
    stop(&mut child).await;
}

/// A companion is stored before its model is bound. A failed binding must not
/// leave a row the host cannot start over.
#[cfg(unix)]
#[tokio::test]
async fn process_starts_over_a_companion_whose_model_was_never_bound() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");

    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let client = ene_rpc::HostServiceClient::new(transport, config);
    // No assignments: the host has no model to bind, and must still store the
    // companion and answer.
    let created = client
        .create_companion(ene_rpc::CreateCompanionRequest {
            name: "Mio".into(),
            mind: "kind".into(),
            ..Default::default()
        })
        .await
        .expect("a companion whose model is not bound yet is still created");
    let companion = created.view().id.to_owned();
    assert_eq!(created.view().name, "Mio");
    stop(&mut child).await;

    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let client = ene_rpc::HostServiceClient::new(transport, config);
    let snapshot = client
        .get_snapshot(ene_rpc::Empty::default())
        .await
        .expect("the host starts over a companion without a model");
    assert!(
        snapshot
            .view()
            .health
            .as_option()
            .is_some_and(|health| health.ready),
        "the restarted host reports itself ready"
    );
    let companions = client
        .list_companions(ene_rpc::Empty::default())
        .await
        .unwrap();
    assert!(
        companions
            .view()
            .items
            .iter()
            .any(|item| item.id == companion),
        "the companion the failed binding left behind is still listed"
    );
    stop(&mut child).await;
}

/// The client helper opens a full-duplex stream: the host answers while the
/// caller's input stream is still open. The voice input contract names
/// HTTP/2, and the host's plain listener accepts h2c prior knowledge.
#[cfg(unix)]
#[tokio::test]
async fn process_client_opens_the_bidirectional_voice_stream() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    install_voice_stt(&state);
    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let settings = ene_rpc::SettingsServiceClient::new(transport.clone(), config.clone());
    let host = ene_rpc::HostServiceClient::new(transport.clone(), config.clone());
    let clients = ene_rpc::ClientServiceClient::new(transport.clone(), config.clone());
    let voice = ene_rpc::VoiceServiceClient::new(transport, config);
    settings
        .update_settings(ene_rpc::UpdateSettingsRequest {
            expected_revision: 1,
            connections: vec![ene_rpc::ConnectionInput {
                id: "stt".into(),
                name: "stt".into(),
                enabled: true,
                kind: ene_rpc::ConnectionKind::CONNECTION_KIND_STT.into(),
                config: Some(ene_rpc::ConnectionConfig {
                    config: Some(ene_rpc::connection_config::Config::Stt(Box::new(
                        ene_rpc::SttConnection {
                            endpoint: Some(ene_rpc::ConnectionEndpoint {
                                adapter: "voice-stt".into(),
                                endpoint: "https://plugin.example".into(),
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
    let companion = host
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
    clients
        .activate(ene_rpc::ActivationRequest {
            companion_id: companion,
            client_id: "c1".into(),
            ..Default::default()
        })
        .await
        .expect("the client becomes active");
    let mut call = voice
        .stream_audio_input()
        .await
        .expect("the microphone stream opens");
    call.send(ene_rpc::AudioInputChunk {
        client_id: "c1".into(),
        pcm_data: vec![0; 1_024],
        sample_rate: 24_000,
        ..Default::default()
    })
    .await
    .expect("an audio frame");
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        call.message::<ene_rpc::TranscriptionEvent>(),
    )
    .await
    .expect("the host answers before the input stream closes")
    .expect("the answer is not a transport failure")
    .expect("the host sends a frame");
    assert!(answered.view().is_speaking);
    stop(&mut child).await;
}
