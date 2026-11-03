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
