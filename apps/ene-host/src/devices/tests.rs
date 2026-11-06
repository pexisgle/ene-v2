use super::*;

async fn connected() -> (Devices, String, Subscription, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("devices.db")).await.unwrap();
    let devices = Devices::new(store);
    let registration = devices
        .register(w::RegisterDeviceRequest {
            name: "fixture".into(),
            capabilities: vec!["write".into(), "companion.display".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let id = registration.device.id.clone();
    devices.approve(id.clone()).await.unwrap();
    devices
        .permission(id.clone(), Some("write".into()), true)
        .await
        .unwrap();
    let subscription = devices.subscribe(id.clone()).await.unwrap();
    (devices, id, subscription, temp)
}
fn request(id: &str, operation: &str) -> w::DeviceOperationRequest {
    w::DeviceOperationRequest {
        device_id: id.into(),
        operation_id: operation.into(),
        capability: "write".into(),
        input: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}
fn completed(id: &str, operation: &w::DeviceOperation) -> w::DeviceOperationResult {
    w::DeviceOperationResult {
        device_id: id.into(),
        operation_id: operation.operation_id.clone(),
        subscription_id: operation.subscription_id.clone(),
        success: true,
        output: w::JsonDocument {
            json: "\"done\"".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_admitted_operation_is_durable_before_the_device_sees_it() {
    let (devices, id, mut subscription, _temp) = connected().await;
    let admitted = devices.request(request(&id, "first")).await.unwrap();
    assert_eq!(admitted.status, w::OperationStatus::Dispatched);
    let operation = subscription.queue.recv().await.unwrap();
    // The same request replays the admitted record instead of dispatching twice.
    assert_eq!(
        devices.request(request(&id, "first")).await.unwrap(),
        admitted
    );
    let done = devices.complete(completed(&id, &operation)).await.unwrap();
    assert_eq!(done.status, w::OperationStatus::Completed);
    assert_eq!(
        devices.complete(completed(&id, &operation)).await.unwrap(),
        done
    );
    let mut other = completed(&id, &operation);
    other.output = w::JsonDocument {
        json: "\"other\"".into(),
        ..Default::default()
    }
    .into();
    assert!(devices.complete(other).await.is_err());
    assert_eq!(devices.operation(&id, "first").await.unwrap(), done);
    assert!(subscription.queue.try_recv().is_err());
}

#[tokio::test]
async fn revoking_a_capability_revokes_its_dispatched_operations() {
    let (devices, id, mut subscription, _temp) = connected().await;
    devices.request(request(&id, "first")).await.unwrap();
    let operation = subscription.queue.recv().await.unwrap();
    devices
        .permission(id.clone(), Some("write".into()), false)
        .await
        .unwrap();
    assert_eq!(
        devices.operation(&id, "first").await.unwrap().status,
        w::OperationStatus::Revoked
    );
    assert!(devices.complete(completed(&id, &operation)).await.is_err());
    // Without the capability a new operation is refused before dispatch.
    assert!(devices.request(request(&id, "second")).await.is_err());
    assert!(subscription.queue.try_recv().is_err());
}

#[tokio::test]
async fn a_restarted_host_reports_dispatched_operations_as_uncertain() {
    let (devices, id, mut subscription, temp) = connected().await;
    devices.request(request(&id, "first")).await.unwrap();
    assert_eq!(
        subscription.queue.recv().await.unwrap().operation_id,
        "first"
    );
    let store = Store::open(temp.path().join("devices.db")).await.unwrap();
    let restarted = Devices::new(store);
    assert_eq!(
        restarted.operation(&id, "first").await.unwrap().status,
        w::OperationStatus::Uncertain
    );
}

#[tokio::test]
async fn an_activation_revision_survives_a_restart_and_blocks_reuse() {
    let (devices, id, _subscription, temp) = connected().await;
    devices
        .permission(id.clone(), Some("companion.display".into()), true)
        .await
        .unwrap();
    let first = devices
        .activate(w::ActivationRequest {
            companion_id: "companion".into(),
            device_id: id.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    let store = Store::open(temp.path().join("devices.db")).await.unwrap();
    let restarted = Devices::new(store);
    assert!(
        restarted
            .activate(w::ActivationRequest {
                companion_id: "companion".into(),
                device_id: id,
                expected_revision: Some(first.revision),
                ..Default::default()
            })
            .await
            .is_err(),
        "a restart must not let an unrenewed activation continue"
    );
}
