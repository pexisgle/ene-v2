use super::*;

#[test]
fn persisted_summaries_resume_without_rewriting_source_messages() {
    let temp = tempfile::tempdir().unwrap();
    let store = rig_memory::DirStore::open(temp.path()).unwrap();
    let mut source = conversation(3);
    let original_messages = source.messages().to_vec();
    let original_ids: Vec<_> = source.entries().map(|entry| entry.id).collect();
    let older = Summary {
        text: "one completed turn".into(),
        through_message_id: original_ids[3].get(),
    };
    source.add_aside(Aside::new(
        SUMMARY,
        Author::plugin("ene-host"),
        serde_json::to_value(&older).unwrap(),
    ));
    rig_memory::save(&store, "main", &mut source).unwrap();
    let older_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
    let mut source = rig_memory::load(&store, "main").unwrap();
    assert_eq!(summary_of(&source), Some(older));

    let latest = Summary {
        text: "two completed turns".into(),
        through_message_id: original_ids[7].get(),
    };
    source.add_aside(Aside::new(
        SUMMARY,
        Author::plugin("ene-host"),
        serde_json::to_value(&latest).unwrap(),
    ));
    rig_memory::save(&store, "main", &mut source).unwrap();
    let latest_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
    assert!(latest_bytes.len() > older_bytes.len());
    assert!(latest_bytes.starts_with(&older_bytes));
    drop(store);

    let store = rig_memory::DirStore::open(temp.path()).unwrap();
    let source = rig_memory::load(&store, "main").unwrap();
    let resumed = summary_of(&source).expect("the newest saved summary");
    assert_eq!(resumed, latest);
    assert_eq!(source.asides().count(), 2);
    assert_eq!(source.messages(), original_messages);
    assert_eq!(
        source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
        original_ids
    );

    let mut request = source.view();
    inject_summary(&mut request, &resumed);
    assert!(matches!(
        request.entries().next().unwrap().message,
        Message::System { content } if content.contains("two completed turns")
            && !content.contains("one completed turn")
    ));
    assert_eq!(&request.messages()[1..], &original_messages[8..]);
    assert_eq!(
        request
            .entries()
            .skip(1)
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        original_ids[8..]
    );
    request
        .validate()
        .expect("the kept tool call has its result");
    assert_eq!(source.messages(), original_messages);
    assert_eq!(
        source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
        original_ids
    );

    let persisted = rig_memory::load(&store, "main").unwrap();
    assert_eq!(persisted, source);
    assert_eq!(persisted.messages(), original_messages);
    assert_eq!(
        persisted
            .entries()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(summary_of(&persisted), Some(latest));
    assert_eq!(
        rig_memory::Store::read_log(&store, "main").unwrap(),
        latest_bytes
    );
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
