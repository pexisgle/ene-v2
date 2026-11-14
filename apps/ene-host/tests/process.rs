//! A real host process: private discovery, owner auth and graceful SIGTERM.
#![cfg(unix)]

use std::{
    path::Path,
    process::{Child, Command},
    time::Duration,
};

fn connection_file(runtime: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(runtime.join("connection.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        value.get("endpoint")?.as_str()?.to_owned(),
        value.get("token")?.as_str()?.to_owned(),
    ))
}

fn spawn(state: &Path, runtime: &Path) -> Child {
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

async fn stop(child: &mut Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let exited = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(exited.is_ok(), "host did not stop");
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
