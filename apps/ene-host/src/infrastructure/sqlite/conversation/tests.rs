use super::*;
use crate::infrastructure::sqlite::Store;
#[tokio::test]
async fn storage_failure_does_not_leave_partial_messages() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).await.unwrap();
    store.run(|conn| {
        let c=load(conn)?.companions.into_values().next().unwrap().id;
        accept(conn,"key",&c,json!({"text":"日本🙂"}))?;
        conn.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(FAIL,'disk full'); END;").unwrap();
        assert!(accept(conn,"failed-key",&c,json!({"text":"書込失敗"})).is_err());
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM messages",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        Ok(())
    }).await.unwrap();
}
#[tokio::test]
async fn restart_preserves_identity_partial_messages_and_finalizes_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let first = Store::open(path.clone()).await.unwrap();
    let before = first.snapshot().await.unwrap();
    let (principal, receipts) = first
        .run(|db| {
            let c = load(db)?.companions.into_values().next().unwrap().id;
            let queued = accept(db, "queued", &c, json!({"text":"queued"}))?.receipt;
            let streaming = accept(db, "streaming", &c, json!({"text":"streaming"}))?.receipt;
            update_message(
                db,
                &streaming.response_message_id,
                MessageStatus::Streaming,
                Some("生成途中🙂".into()),
                None,
            )?;
            save_receipt(
                db,
                "check",
                "/v1/connections/p/check",
                &json!({"purpose":"companion"}),
                0,
                &json!({"request_id":"original"}),
            )?;
            Ok((super::super::meta(db, "principal")?, [queued, streaming]))
        })
        .await
        .unwrap();
    first.db.clone().close().await.unwrap();
    drop(first);
    let second = Store::open(path).await.unwrap();
    let after = second.snapshot().await.unwrap();
    assert_eq!(before.host_id, after.host_id);
    assert_eq!(before.environment_id, after.environment_id);
    assert_eq!(before.companions, after.companions);
    assert_eq!(after.messages.len(), 4);
    second
        .run(move |db| {
            assert_eq!(principal, super::super::meta(db, "principal")?);
            let c = load(db)?.companions.into_values().next().unwrap().id;
            for ((key, partial), original) in [("queued", ""), ("streaming", "生成途中🙂")]
                .into_iter()
                .zip(receipts)
            {
                let replay = accept(db, key, &c, json!({"text":key}))?;
                assert_eq!(replay.receipt.request_id, original.request_id);
                assert_eq!(replay.receipt.message_id, original.message_id);
                assert_eq!(
                    replay.receipt.response_message_id,
                    original.response_message_id
                );
                assert!(!replay.created);
                let message = super::super::page(db, &c, None, 50)?
                    .items
                    .into_iter()
                    .find(|m| m.id == original.response_message_id)
                    .unwrap();
                assert_eq!(message.text, partial);
                assert_eq!(message.status, MessageStatus::Failed);
                assert_eq!(message.failure.unwrap().code, "host_interrupted");
            }
            let check = receipt(
                db,
                "check",
                "/v1/connections/p/check",
                &json!({"purpose":"companion"}),
            )?
            .unwrap();
            assert_eq!(check.0, 503);
            assert_eq!(check.1["request_id"], "original");
            assert_eq!(check.1["error"]["details"]["reason"], "host_interrupted");
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(second.snapshot().await.unwrap().messages.len(), 4);
}
