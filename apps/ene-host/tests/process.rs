//! A real host process: private discovery, owner auth and graceful SIGTERM.
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

/// A conversation and its acceptance receipt survive a restart: the restored
/// agent keeps its role, and a retry with the same request key is not run
/// again.
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
    settings
        .register_credentials(ene_rpc::CredentialRequest {
            id: "c1".into(),
            secret: Some("dummy-key".into()),
            ..Default::default()
        })
        .await
        .expect("the credential is stored for this session");
    let companion = host
        .create_companion(ene_rpc::CreateCompanionRequest {
            name: "Mio".into(),
            mind: "kind".into(),
            assignments: Some(ene_rpc::ModelAssignments {
                companion: Some(assignment.clone()).into(),
                task: Some(assignment).into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })
        .await
        .expect("a companion")
        .view()
        .id
        .to_owned();
    let accepted = chat
        .send_message(ene_rpc::SendMessageRequest {
            companion_id: companion.clone(),
            request_key: "k1".into(),
            text: "覚えていて".into(),
            ..Default::default()
        })
        .await
        .expect("the message is accepted")
        .view()
        .request_id
        .to_owned();
    stop(&mut child).await;

    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = ene_rpc::connect(&endpoint, &token).unwrap();
    let settings = ene_rpc::SettingsServiceClient::new(transport.clone(), config.clone());
    let chat = ene_rpc::ChatServiceClient::new(transport, config);
    // The credential is session-only, so it is gone after the restart.
    settings
        .register_credentials(ene_rpc::CredentialRequest {
            id: "c1".into(),
            secret: Some("dummy-key".into()),
            ..Default::default()
        })
        .await
        .expect("the credential is stored for this session");
    let restored = chat
        .get_history(ene_rpc::HistoryRequest {
            companion_id: companion.clone(),
            ..Default::default()
        })
        .await
        .expect("the restored history is readable");
    let restored = restored
        .view()
        .items
        .iter()
        .map(|item| item.text)
        .collect::<Vec<_>>();
    assert_eq!(
        restored,
        ["覚えていて"],
        "the accepted input survives the restart"
    );
    let retried = chat
        .send_message(ene_rpc::SendMessageRequest {
            companion_id: companion.clone(),
            request_key: "k1".into(),
            text: "覚えていて".into(),
            ..Default::default()
        })
        .await
        .expect("the retry is accepted")
        .view()
        .request_id
        .to_owned();
    assert_eq!(
        retried, accepted,
        "the retry binds to the restored acceptance instead of running again"
    );
    let history = chat
        .get_history(ene_rpc::HistoryRequest {
            companion_id: companion,
            ..Default::default()
        })
        .await
        .expect("the history is readable");
    let history = history
        .view()
        .items
        .iter()
        .filter(|item| item.text == "覚えていて")
        .count();
    assert_eq!(history, 1, "the retry does not add a second input");
    stop(&mut child).await;
}

/// A companion is stored before its model is bound. A failed binding must not
/// leave a row the host cannot start over.
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
