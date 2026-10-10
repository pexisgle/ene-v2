use super::*;
use ene_core::domain::Role;

#[test]
fn pagination_is_scoped_ordered_and_keeps_old_active_messages() {
    let mut db = Connection::open_in_memory().unwrap();
    initialize(&mut db).unwrap();
    let c = snapshot(&mut db).unwrap().companions[0].id.clone();
    for n in 1..=61 {
        let status = if n == 1 {
            MessageStatus::Streaming
        } else {
            MessageStatus::Completed
        };
        let m = Message {
            id: format!("m{n}"),
            companion_id: c.clone(),
            role: Role::Companion,
            text: format!("メモ{n}"),
            status: status.clone(),
            created_at: "2026-10-01T00:00:00Z".into(),
            sequence: n,
            resources: vec![],
            in_reply_to: None,
            task_id: None,
            question_id: None,
            failure: None,
        };
        db.execute(
            "INSERT INTO messages VALUES (?1,?2,?3,?4,?5)",
            params![
                m.id,
                c,
                n as i64,
                if status == MessageStatus::Streaming {
                    "streaming"
                } else {
                    "completed"
                },
                serde_json::to_string(&m).unwrap()
            ],
        )
        .unwrap();
    }
    let p = page(&db, &c, None, 50).unwrap();
    assert_eq!(p.items.first().unwrap().sequence, 12);
    assert_eq!(p.items.last().unwrap().sequence, 61);
    let older = page(&db, &c, p.next_cursor.as_deref(), 50).unwrap();
    assert_eq!(older.items.len(), 11);
    assert!(older.next_cursor.is_none());
    assert!(page(&db, "other", p.next_cursor.as_deref(), 50).is_err());
    let s = snapshot(&mut db).unwrap();
    assert_eq!(s.messages.len(), 51);
    assert_eq!(s.messages[0].sequence, 1);
}

#[test]
fn escaped_history_is_bounded_without_losing_rows_or_active_replies() {
    let mut db = Connection::open_in_memory().unwrap();
    initialize(&mut db).unwrap();
    let companion = snapshot(&mut db).unwrap().companions[0].id.clone();
    for sequence in 1..=50 {
        let active = sequence <= 8;
        let message = Message {
            id: format!("large-{sequence}"),
            companion_id: companion.clone(),
            role: Role::Companion,
            text: "\0".repeat(128_000),
            status: if active {
                MessageStatus::Streaming
            } else {
                MessageStatus::Completed
            },
            created_at: "2026-10-01T00:00:00Z".into(),
            sequence,
            resources: vec![],
            in_reply_to: None,
            task_id: None,
            question_id: None,
            failure: None,
        };
        db.execute(
            "INSERT INTO messages VALUES (?1,?2,?3,?4,?5)",
            params![
                message.id,
                companion,
                sequence as i64,
                if active { "streaming" } else { "completed" },
                serde_json::to_string(&message).unwrap(),
            ],
        )
        .unwrap();
    }
    let recent = snapshot(&mut db).unwrap();
    assert!(serde_json::to_vec(&recent.messages).unwrap().len() < 16 * 1024 * 1024);
    for sequence in 1..=8 {
        assert!(recent.messages.iter().any(|m| m.sequence == sequence));
    }
    assert!(recent.message_cursors[&companion].is_some());
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let history = page(&db, &companion, cursor.as_deref(), 100).unwrap();
        assert!(serde_json::to_vec(&history).unwrap().len() < 16 * 1024 * 1024);
        assert!(!history.items.is_empty());
        for message in &history.items {
            assert_eq!(message.text, "\0".repeat(128_000));
            assert!(seen.insert(message.sequence), "page repeated a row");
        }
        match history.next_cursor {
            Some(next) => {
                assert_ne!(cursor.as_ref(), Some(&next));
                cursor = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(seen, (1..=50).collect());
}

#[test]
fn snapshot_budget_is_global_and_keeps_active_inputs_with_lossless_cursors() {
    let mut db = Connection::open_in_memory().unwrap();
    initialize(&mut db).unwrap();
    let mut config = conversation::load(&db).unwrap();
    let original = config.companions.values().next().unwrap().clone();
    config.companions.clear();
    for index in 0..6 {
        let mut companion = original.clone();
        companion.id = format!("c{index}");
        config.companions.insert(companion.id.clone(), companion);
    }
    conversation::write_configuration(&db, &config).unwrap();
    for companion in config.companions.keys() {
        for sequence in 1..=14 {
            let message: Message = serde_json::from_value(json!({"id":format!("{companion}-{sequence}"),"companion_id":companion,"role":if sequence == 1 {"user"} else {"companion"},"text":"\0".repeat(128_000),"status":if sequence == 2 {"streaming"} else {"completed"},"sequence":sequence,"created_at":"now","in_reply_to":if sequence == 2 {Some(format!("{companion}-1"))} else {None},"resources":[],"task_id":null,"question_id":null,"failure":null})).unwrap();
            db.execute(
                "INSERT INTO messages VALUES (?1,?2,?3,?4,?5)",
                params![
                    message.id,
                    companion,
                    sequence,
                    if sequence == 2 {
                        "streaming"
                    } else {
                        "completed"
                    },
                    serde_json::to_string(&message).unwrap()
                ],
            )
            .unwrap();
        }
    }
    let snapshot = snapshot(&mut db).unwrap();
    assert!(
        ene_api_types::budget::encoded_len(
            &snapshot.messages,
            ene_api_types::budget::MAX_RESPONSE_BYTES
        )
        .is_ok()
    );
    for companion in config.companions.keys() {
        for sequence in [1, 2] {
            assert!(
                snapshot
                    .messages
                    .iter()
                    .any(|message| message.id == format!("{companion}-{sequence}"))
            );
        }
        let mut seen: std::collections::BTreeSet<_> = snapshot
            .messages
            .iter()
            .filter(|message| &message.companion_id == companion)
            .map(|message| message.sequence)
            .collect();
        let mut cursor = snapshot.message_cursors[companion].clone();
        assert!(cursor.is_some());
        while let Some(before) = cursor {
            let page = page(&db, companion, Some(&before), 50).unwrap();
            assert!(
                ene_api_types::budget::encoded_len(
                    &page,
                    ene_api_types::budget::MAX_HISTORY_PAGE_BYTES
                )
                .is_ok()
            );
            for message in page.items {
                assert_eq!(message.text.len(), 128_000);
                seen.insert(message.sequence);
            }
            cursor = page.next_cursor;
        }
        assert_eq!(seen, (1..=14).collect());
    }
}

#[tokio::test]
async fn completing_check_requires_matching_pending_identity_and_updates_one_receipt() {
    use ene_core::ports::{ConfigurationCommit, Outcome, Receipt, Repository};
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
