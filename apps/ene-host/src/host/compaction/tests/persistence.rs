use super::*;

#[test]
fn the_checkpoint_restores_without_modifying_the_log() {
    let temp = tempfile::tempdir().unwrap();
    let database = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    database.companion_repo().seed_initial_companion().unwrap();
    let logs = rig_memory::DirStore::open(temp.path().join("rig")).unwrap();
    let mut history = conversation(3);
    rig_memory::save(&logs, "main", &mut history).unwrap();
    let checkpoint = Summary {
        text: "checkpoint summary".to_owned(),
        through_message_id: history.entries().nth(7).unwrap().id.get(),
    };
    database
        .conn()
        .execute(
            "INSERT INTO compaction_checkpoints VALUES ('ene', ?1, ?2, ?3)",
            (
                log_lineage(&history).unwrap(),
                checkpoint.through_message_id,
                &checkpoint.text,
            ),
        )
        .unwrap();
    let bytes = rig_memory::Store::read_log(&logs, "main").unwrap();
    let restored = rig_memory::load(&logs, "main").unwrap();
    assert_eq!(
        load_summary(&database, "ene", &restored)
            .unwrap()
            .unwrap()
            .0,
        checkpoint
    );
    assert_eq!(rig_memory::Store::read_log(&logs, "main").unwrap(), bytes);
}

#[test]
fn injected_blocks_are_system_and_never_two_user_messages() {
    let mut history = conversation(6);
    let through = history.entries().nth(3).unwrap().id.get();
    let summary = Summary {
        text: "これまでの流れ: テスト".to_owned(),
        through_message_id: through,
    };
    inject_summary(&mut history, &summary);
    history.validate().expect("the summary keeps tool pairs");
    assert!(head_text(&history).starts_with(SUMMARY_HEAD));
    assert!(fit(
        &mut history,
        &FitRequest {
            model: "",
            prefix: &empty_request(),
            anchor: None,
            sent: None,
            budget: 650,
            turn_start: None,
        },
    ));
    history.validate().expect("the notice keeps tool pairs");
    assert!(
        head_text(&history).starts_with(SUMMARY_HEAD),
        "the summary head is kept"
    );
    let messages: Vec<&Message> = history.messages().iter().map(Arc::as_ref).collect();
    for pair in messages.windows(2) {
        assert!(
            !(matches!(pair[0], Message::User { .. }) && matches!(pair[1], Message::User { .. })),
            "injection never puts two user messages in a row"
        );
    }
    let notices = history
        .entries()
        .filter(|entry| matches!(entry.message, Message::System { content } if content == NOTICE))
        .count();
    assert_eq!(notices, 1, "the notice is a system message");
}
