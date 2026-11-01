use super::*;

async fn connected() -> (Devices, String, Subscription) {
    let devices = Devices::new(None);
    let registration = devices
        .register(w::RegisterDeviceRequest {
            name: "fixture".into(),
            capabilities: vec!["write".into()],
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
    (devices, id, subscription)
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
async fn fill(devices: &Devices, remaining: usize) {
    let mut state = devices.lock().await.unwrap();
    // Represent a full persisted journal without thousands of test operations.
    state.journal.devices[0]
        .device
        .as_option_mut()
        .unwrap()
        .name = "x".repeat(MAX_JOURNAL_BYTES - remaining - 512);
}
#[tokio::test]
async fn admitted_write_can_persist_a_maximum_result_near_capacity() {
    let (devices, id, mut subscription) = connected().await;
    fill(&devices, COMPLETION_RESERVATION + 512).await;
    devices.request(request(&id, "first")).await.unwrap();
    let operation = subscription.queue.recv().await.unwrap();
    let mut result = w::DeviceOperationResult {
        device_id: id.clone(),
        operation_id: operation.operation_id,
        subscription_id: operation.subscription_id,
        success: true,
        output: w::JsonDocument {
            json: "\"\"".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let overhead = result.encoded_len() as usize;
    result.output.as_option_mut().unwrap().json =
        format!("\"{}\"", "x".repeat(MAX_RESULT_BYTES - overhead - 4));
    assert!(result.encoded_len() as usize <= MAX_RESULT_BYTES);
    assert!(result.encoded_len() as usize >= MAX_RESULT_BYTES - 8);
    let completed = devices.complete(result.clone()).await.unwrap();
    assert_eq!(completed.status, w::OperationStatus::Completed);
    assert_eq!(devices.complete(result).await.unwrap(), completed);
    assert_eq!(devices.operation(&id, "first").await.unwrap(), completed);
}
#[tokio::test]
async fn insufficient_completion_capacity_rejects_before_delivery() {
    let (devices, id, mut subscription) = connected().await;
    fill(&devices, 8 * 1024).await;
    assert!(devices.request(request(&id, "first")).await.is_err());
    assert!(matches!(
        subscription.queue.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(devices.lock().await.unwrap().journal.operations.is_empty());
}
#[tokio::test]
async fn other_writes_cannot_consume_an_admitted_operations_reservation() {
    let (devices, id, mut subscription) = connected().await;
    fill(&devices, COMPLETION_RESERVATION + 512).await;
    devices.request(request(&id, "first")).await.unwrap();
    assert!(devices.request(request(&id, "second")).await.is_err());
    assert_eq!(
        subscription.queue.recv().await.unwrap().operation_id,
        "first"
    );
    assert!(subscription.queue.try_recv().is_err());
    assert_eq!(devices.lock().await.unwrap().journal.operations.len(), 1);
}
