use super::*;

#[tokio::test]
async fn completing_check_requires_matching_pending_identity_and_updates_one_receipt() {
    use crate::ports::{ConfigurationCommit, Outcome, Receipt};
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("receipt.db")).await.unwrap();
    let original = store.configuration().await.unwrap();
    let pending = Receipt {
        key: "check".into(),
        target: "/v1/connections/p/check".into(),
        request: json!({"companion_id":"c"}),
        outcome: Outcome::Pending,
        response: json!({"request_id":"owned"}),
    };
    store.reserve(pending.clone()).await.unwrap();
    for mutation in 0..4 {
        let mut receipt = pending.clone();
        receipt.outcome = Outcome::Completed;
        receipt.response = json!({"request_id":"owned","success":true});
        match mutation {
            0 => receipt.key = "missing".into(),
            1 => receipt.target = "/other".into(),
            2 => receipt.request = json!({"companion_id":"other"}),
            3 => receipt.response["request_id"] = json!("other"),
            _ => unreachable!(),
        }
        let mut configuration = original.clone();
        configuration.settings.revision = 900;
        assert!(
            store
                .commit_configuration(ConfigurationCommit::CompleteCheck {
                    configuration,
                    receipt
                })
                .await
                .is_err()
        );
        assert_eq!(
            store.configuration().await.unwrap().settings,
            original.settings
        );
        assert_eq!(
            store
                .receipt(
                    pending.key.clone(),
                    pending.target.clone(),
                    pending.request.clone()
                )
                .await
                .unwrap()
                .unwrap()
                .0,
            Outcome::Pending
        );
    }
    let mut receipt = pending;
    receipt.outcome = Outcome::Completed;
    receipt.response = json!({"request_id":"owned","success":true});
    let mut configuration = original;
    configuration.settings.revision += 1;
    for _ in 0..2 {
        store
            .commit_configuration(ConfigurationCommit::CompleteCheck {
                configuration: configuration.clone(),
                receipt: receipt.clone(),
            })
            .await
            .unwrap();
    }
    assert_eq!(
        store.configuration().await.unwrap().settings.revision,
        configuration.settings.revision
    );
}

#[tokio::test]
async fn a_legacy_device_journal_is_migrated_into_rows_once() {
    use buffa::Message;
    use ene_rpc::rpc as w;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("devices.db")).await.unwrap();
    let device = w::Device {
        id: "device".into(),
        name: "legacy".into(),
        approved: true,
        capabilities: vec!["write".into()],
        permissions: vec!["write".into()],
        ..Default::default()
    };
    let request = w::DeviceOperationRequest {
        device_id: "device".into(),
        operation_id: "operation".into(),
        capability: "write".into(),
        ..Default::default()
    };
    let journal = w::DeviceJournal {
        devices: vec![w::DeviceRecord {
            device: device.into(),
            token_hash: vec![1, 2, 3],
            ..Default::default()
        }],
        operations: vec![w::OperationRecord {
            request: request.into(),
            state: w::DeviceOperationState {
                operation_id: "operation".into(),
                status: w::OperationStatus::Dispatched.into(),
                ..Default::default()
            }
            .into(),
            subscription_id: "subscription".into(),
            ..Default::default()
        }],
        activation_revision: 5,
        ..Default::default()
    };
    let bytes = journal.encode_to_vec();
    store
        .run(move |db| {
            db.execute(
                "INSERT INTO rpc_state VALUES ('devices',?1)",
                params![bytes],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    store.device_migrate_legacy().await.unwrap();
    store.device_migrate_legacy().await.unwrap();

    assert_eq!(
        store
            .device_get("device".into())
            .await
            .unwrap()
            .unwrap()
            .name,
        "legacy"
    );
    assert_eq!(store.device_operations().await.unwrap(), 1);
    assert_eq!(
        store
            .device_operation("operation".into())
            .await
            .unwrap()
            .unwrap()
            .state
            .as_option()
            .unwrap()
            .status,
        w::OperationStatus::Dispatched
    );
    let legacy = store
        .run(|db| {
            let body: Option<Vec<u8>> = db
                .query_row("SELECT body FROM rpc_state WHERE key='devices'", [], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(body)
        })
        .await
        .unwrap();
    assert!(legacy.is_none());
    assert_eq!(store.device_recover().await.unwrap(), 5);
}
