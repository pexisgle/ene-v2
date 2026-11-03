use super::*;
use crate::infrastructure::sqlite::Store;

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
async fn a_pending_chat_receipt_survives_restart_for_the_next_send_to_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let target = "/v1/companions/c/messages";
    let request = json!({"text": "hello"});
    let (target_first, request_first) = (target.to_owned(), request.clone());
    let first = Store::open(path.clone()).await.unwrap();
    first
        .run(move |db| {
            save_receipt(
                db,
                "chat",
                &target_first,
                &request_first,
                0,
                &json!({"requestId": "r1"}),
            )?;
            save_receipt(
                db,
                "check",
                "/v1/connections/x/check",
                &json!({}),
                0,
                &json!({"requestId": "c1"}),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    first.db.clone().close().await.unwrap();
    drop(first);

    // A chat input may already be in the kernel's saved log, so its receipt
    // is left for the host to settle from the kernel. Other pending receipts
    // are settled as before.
    let (target, request) = (target.to_owned(), request);
    let second = Store::open(path).await.unwrap();
    second
        .run(move |db| {
            let chat = receipt(db, "chat", &target, &request)?.unwrap();
            assert_eq!(chat.0, 0);
            assert_eq!(chat.1["requestId"], "r1");
            let check = receipt(db, "check", "/v1/connections/x/check", &json!({}))?.unwrap();
            assert_eq!(check.0, 503);
            Ok(())
        })
        .await
        .unwrap();
}
