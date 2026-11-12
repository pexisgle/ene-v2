//! A real host process: private discovery, owner auth, and graceful SIGTERM.

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

#[tokio::test]
async fn process_publishes_private_discovery_authenticates_and_stops() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ene-host"))
        .env("ENE_HOST_STATE_DIR", &state)
        .env("ENE_HOST_RUNTIME_DIR", &runtime)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start ene-host");
    let discovery = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(discovery) = connection_file(&runtime) {
                return discovery;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("host did not publish discovery");
    let (transport, config) = ene_rpc::connect(&discovery.0, &discovery.1).unwrap();
    let client = ene_rpc::HostServiceClient::new(transport, config);
    let snapshot = client
        .get_snapshot(ene_rpc::Empty::default())
        .await
        .unwrap();
    assert!(!snapshot.view().server_version.is_empty());
    let (transport, config) = ene_rpc::connect(&discovery.0, "wrong-token").unwrap();
    let wrong = ene_rpc::HostServiceClient::new(transport, config);
    assert!(wrong.get_snapshot(ene_rpc::Empty::default()).await.is_err());
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    let exited = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(exited.is_ok(), "host did not stop");
    assert!(!runtime.join("connection.json").exists());
}
