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
