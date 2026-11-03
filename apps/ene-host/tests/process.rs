#![cfg(unix)]

mod common;

use common::{command, start, stop};
use connectrpc::client::{CallOptions, HttpClient};
use ene_rpc::{HostServiceClient, SnapshotRequest, UpdateCompanionRequest};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::time::timeout;

fn client(connection: &serde_json::Value, token: &str) -> HostServiceClient<HttpClient> {
    let (transport, config) =
        ene_rpc::connect(connection["endpoint"].as_str().unwrap(), token).unwrap();
    HostServiceClient::new(transport, config)
}

fn options() -> CallOptions {
    CallOptions::default().with_timeout(Duration::from_secs(5))
}

#[tokio::test]
async fn host_process_authenticates_persists_and_stops_with_an_open_subscription() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (mut child, connection) = start(root.path()).await;
    let mut duplicate = command(root.path()).spawn().unwrap();
    assert!(
        !timeout(Duration::from_secs(5), duplicate.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let unauthorized = client(&connection, "wrong-token")
        .get_snapshot_with_options(SnapshotRequest::default(), options())
        .await
        .unwrap_err();
    assert_eq!(unauthorized.code, connectrpc::ErrorCode::Unauthenticated);
    let client = client(&connection, connection["token"].as_str().unwrap());
    let snapshot = client
        .get_snapshot_with_options(SnapshotRequest::default(), options())
        .await
        .unwrap()
        .into_owned();
    let companion = &snapshot.companions[0];
    let updated = client
        .update_companion_with_options(
            UpdateCompanionRequest {
                id: companion.id.clone(),
                revision: companion.revision,
                name: Some("再起動しても保存".into()),
                ..Default::default()
            },
            options(),
        )
        .await
        .unwrap()
        .into_owned();
    let mut events = client.subscribe(ene_rpc::Empty::default()).await.unwrap();
    assert!(
        timeout(
            Duration::from_secs(5),
            events.message::<ene_rpc::HostEvent>()
        )
        .await
        .unwrap()
        .unwrap()
        .is_some()
    );
    stop(&mut child, root.path()).await;

    let (mut child, restarted) = start(root.path()).await;
    assert_ne!(connection["token"], restarted["token"]);
    let (transport, config) = ene_rpc::connect(
        restarted["endpoint"].as_str().unwrap(),
        restarted["token"].as_str().unwrap(),
    )
    .unwrap();
    let snapshot = HostServiceClient::new(transport, config)
        .get_snapshot_with_options(SnapshotRequest::default(), options())
        .await
        .unwrap()
        .into_owned();
    assert_eq!(snapshot.companions[0].id, updated.id);
    assert_eq!(snapshot.companions[0].name, updated.name);
    assert_eq!(snapshot.companions[0].revision, updated.revision);
    stop(&mut child, root.path()).await;
}
