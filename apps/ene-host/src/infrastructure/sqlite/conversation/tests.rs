use super::*;
use crate::infrastructure::sqlite::{Store, meta};

#[tokio::test]
async fn a_pending_receipt_completes_once_and_releases_only_before_completion() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).await.unwrap();
    store
        .run(|db| {
            let target = "/v1/companions/c/messages";
            let request = json!({"text": "hello"});
            save_receipt(db, "key", target, &request, 0, &json!({"requestId": "r1"}))?;
            complete_receipt(
                db,
                "key",
                target,
                &request,
                202,
                &json!({"requestId": "r1"}),
            )?;
            // The same completion is idempotent.
            complete_receipt(
                db,
                "key",
                target,
                &request,
                202,
                &json!({"requestId": "r1"}),
            )?;
            let stored = receipt(db, "key", target, &request)?.unwrap();
            assert_eq!(stored.0, 202);
            assert_eq!(stored.1["requestId"], "r1");
            // A different result for the same key is a conflict.
            assert!(
                complete_receipt(
                    db,
                    "key",
                    target,
                    &request,
                    202,
                    &json!({"requestId": "r2"})
                )
                .is_err()
            );
            // A completed receipt is never released for a retry.
            release_receipt(db, "key", target, &request)?;
            assert!(receipt(db, "key", target, &request)?.is_some());

            save_receipt(db, "pending", target, &request, 0, &json!({}))?;
            release_receipt(db, "pending", target, &request)?;
            assert!(receipt(db, "pending", target, &request)?.is_none());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_marks_an_unconfirmed_delivery_unavailable_instead_of_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let first = Store::open(path.clone()).await.unwrap();
    first
        .run(|db| {
            save_receipt(
                db,
                "pending",
                "/v1/companions/c/messages",
                &json!({"text": "hello"}),
                0,
                &json!({"requestId": "r1"}),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    first.db.clone().close().await.unwrap();
    drop(first);
    let second = Store::open(path).await.unwrap();
    second
        .run(|db| {
            let (outcome, response) = receipt(
                db,
                "pending",
                "/v1/companions/c/messages",
                &json!({"text": "hello"}),
            )?
            .unwrap();
            assert_eq!(outcome, 503);
            assert_eq!(response["error"]["code"], "unavailable");
            Ok(())
        })
        .await
        .unwrap();
    let _ = meta;
}
