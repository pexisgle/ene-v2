use crate::{
    domain::{
        Activation, ApprovalRequest, ApprovalStatus, Companion, HostNotification, Message,
        MessageDelta, MessageRole, NotificationEvent, PolicyText, Settings, Snapshot, TurnEvent,
        TurnStatus, WorkStatus,
    },
    reducer::is_user_facing_message,
    state::{Link, Model, Sending},
    update::Update,
};
use ene_domain::CompanionId;
use std::time::Duration;

fn model() -> Model {
    Model::new("connection.json".into())
}

fn snapshot() -> Snapshot {
    Snapshot::default()
}

fn notification(event: NotificationEvent) -> HostNotification {
    HostNotification {
        kind: "test".into(),
        event: Some(event),
    }
}

fn message(id: &str, sequence: u64, role: MessageRole, text: &str) -> Message {
    Message {
        id: id.into(),
        companion_id: Some(CompanionId::parse("ene").unwrap()),
        role,
        text: text.into(),
        sequence,
        ..Default::default()
    }
}

fn with_chat() -> Model {
    let mut model = model();
    model.companions.chat.companion_id = Some(CompanionId::parse("ene").unwrap());
    model
}

#[test]
fn removing_a_companion_updates_the_live_view_and_selects_the_remaining_one() {
    use crate::domain::ClientInfo;

    let mut model = with_chat();
    let ene = CompanionId::parse("ene").unwrap();
    model.companions.selected = Some("ene".into());
    model.snapshot = Some(Snapshot {
        companions: ["ene", "mio"]
            .into_iter()
            .map(|id| Companion {
                id: id.into(),
                ..Default::default()
            })
            .collect(),
        active_companion_id: Some("ene".into()),
        activations: ["ene", "mio"]
            .into_iter()
            .map(|id| Activation {
                companion_id: CompanionId::parse(id).unwrap(),
                client_id: Some("desk".into()),
                generation: None,
            })
            .collect(),
        clients: vec![ClientInfo {
            id: "desk".into(),
            ..Default::default()
        }],
        active_work: [("task".to_owned(), ene.clone())].into(),
        ..snapshot()
    });

    let effects = model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::CompanionRemoved(ene),
    ))));
    assert_eq!(model.companion_id(), Some("mio"));
    assert_eq!(model.resident_companions("desk"), ["mio"]);
    assert!(model.companions.chat.companion_id.is_none());
    assert!(!model.has_active_work());
    let snapshot = model.snapshot.unwrap();
    assert_eq!(snapshot.active_companion_id.as_deref(), Some("mio"));
    assert_eq!(snapshot.clients[0].id, "desk");
    assert!(effects.history && effects.mind);
}

#[test]
fn a_delta_is_a_preview_until_the_host_records_the_message() {
    let mut model = with_chat();
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::MessageDelta(MessageDelta {
            companion_id: "ene".to_owned(),
            stream_id: "stream".into(),
            delta: "こん".into(),
        }),
    ))));
    assert!(
        model.companions.chat.messages.is_empty(),
        "a delta is not a message"
    );
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::MessageDelta(MessageDelta {
            companion_id: "ene".to_owned(),
            stream_id: "stream".into(),
            delta: "にちは".into(),
        }),
    ))));
    assert_eq!(
        model
            .companions
            .streaming
            .get(&("ene".to_owned(), "stream".to_owned()))
            .map(String::as_str),
        Some("こんにちは")
    );
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::MessageCreated(Box::new(message(
            "a",
            9,
            MessageRole::Assistant,
            "こんにちは",
        ))),
    ))));
    assert_eq!(model.companions.chat.messages.len(), 1);
    assert!(
        model.companions.streaming.is_empty(),
        "the preview gives way to the message"
    );
}

#[test]
fn the_same_message_twice_is_one_message_in_sequence_order() {
    let mut model = with_chat();
    let created = |id: &str, sequence: u64| {
        Update::Notification(Box::new(notification(NotificationEvent::MessageCreated(
            Box::new(message(id, sequence, MessageRole::User, id)),
        ))))
    };
    model.apply(created("b", 7));
    model.apply(created("a", 5));
    model.apply(created("b", 7));
    let ids: Vec<&str> = model
        .companions
        .chat
        .messages
        .iter()
        .map(|message| message.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["a", "b"],
        "replay replaces, and order follows sequence"
    );
}

#[test]
fn a_turn_event_for_another_companion_does_not_touch_the_shown_chat() {
    let mut model = with_chat();
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::TurnEvent(Box::new(TurnEvent {
            companion_id: Some(CompanionId::parse("other").unwrap()),
            turn_id: "1".into(),
            status: TurnStatus::Failed,
            ..Default::default()
        })),
    ))));
    assert!(model.companions.chat.turn.is_none());
}

#[test]
fn a_terminal_turn_drops_the_preview() {
    let mut model = with_chat();
    model
        .companions
        .streaming
        .insert(("ene".into(), "stream".into()), "半分".into());
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::TurnEvent(Box::new(TurnEvent {
            companion_id: Some(CompanionId::parse("ene").unwrap()),
            turn_id: "1".into(),
            status: TurnStatus::Stopped,
            ..Default::default()
        })),
    ))));
    assert!(
        model.companions.streaming.is_empty(),
        "a stopped turn has no preview"
    );
}

#[test]
fn a_resolved_approval_leaves_the_pending_list() {
    let mut model = model();
    model.snapshot = Some(snapshot());
    let pending = |status: ApprovalStatus| ApprovalRequest {
        request_id: "r1".into(),
        status: Some(status),
        ..Default::default()
    };
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::ApprovalRequested(Some(Box::new(pending(ApprovalStatus::Pending)))),
    ))));
    assert_eq!(model.approvals().len(), 1);
    model.apply(Update::Resolved {
        request_id: "r1".into(),
        rule_warning: None,
    });
    assert!(
        model.approvals().is_empty(),
        "a resolved approval is not pending"
    );
    model.apply(Update::Resolved {
        request_id: "r2".into(),
        rule_warning: Some("rule save failed".into()),
    });
    assert!(
        model
            .notices
            .iter()
            .any(|notice| notice.message == "rule save failed"),
        "AlwaysApprove save failures must be surfaced"
    );
}

#[test]
fn f16_policy_texts_updated_notification_updates_settings() {
    let mut model = model();
    model.snapshot = Some(snapshot());
    let entry = PolicyText {
        id: "pt-1".into(),
        text: "外部送信は毎回確認".into(),
        ..Default::default()
    };
    model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::PolicyTextsUpdated(vec![entry]),
    ))));
    assert_eq!(model.policy_texts().len(), 1);
    assert_eq!(model.policy_texts()[0].text, "外部送信は毎回確認");
}

#[test]
fn an_older_companion_revision_never_replaces_a_newer_one() {
    let mut model = model();
    model.snapshot = Some(snapshot());
    let companion = |revision: u64, name: &str| Companion {
        id: "ene".into(),
        name: name.into(),
        revision,
        ..Default::default()
    };
    let update = |revision: u64, name: &str| {
        Update::Notification(Box::new(notification(NotificationEvent::Companion(
            Box::new(companion(revision, name)),
        ))))
    };
    model.apply(update(2, "新しい"));
    model.apply(update(1, "古い"));
    let name = &model.snapshot().unwrap().companions[0].name;
    assert_eq!(name, "新しい");
}

#[test]
fn older_settings_are_ignored_so_a_replay_cannot_roll_the_window_back() {
    let mut model = model();
    model.snapshot = Some(snapshot());
    let settings = |revision: u64| Settings {
        revision,
        ..Default::default()
    };
    let update = |revision: u64| {
        Update::Notification(Box::new(notification(NotificationEvent::SettingsUpdated(
            Some(Box::new(settings(revision))),
        ))))
    };
    model.apply(update(4));
    model.apply(update(3));
    assert_eq!(model.settings().unwrap().revision, 4);
}

#[test]
fn losing_the_link_keeps_the_data_but_moves_in_flight_sends_to_unsent() {
    let mut model = with_chat();
    model.companions.chat.sending.push(Sending {
        request_key: "key".into(),
        text: "送った".into(),
    });
    model.apply(Update::Link(Box::new(Link::Lost {
        reason: "closed".into(),
        retry_in: Duration::from_secs(1),
    })));
    assert!(!model.connections.link.is_live());
    assert_eq!(model.companions.chat.sending.len(), 0);
    assert_eq!(model.companions.chat.unsent.len(), 1);
    assert_eq!(
        model.companions.chat.unsent[0].request_key, "key",
        "the key survives so a retry is idempotent"
    );
}

#[test]
fn a_late_failure_does_not_clobber_new_text() {
    let mut model = with_chat();
    model.companions.chat.sending.push(Sending {
        request_key: "key".into(),
        text: "前の入力".into(),
    });
    model.apply(Update::SendFailed {
        request_key: "key".into(),
        text: "前の入力".into(),
        reason: "保存されませんでした".into(),
    });
    assert!(
        model
            .companions
            .chat
            .unsent
            .iter()
            .any(|unsent| unsent.text == "前の入力")
    );
    assert_eq!(model.companions.chat.sending.len(), 0);
}

#[test]
fn a_recorded_user_message_closes_its_pending_row() {
    let mut model = with_chat();
    model.companions.chat.sending.push(Sending {
        request_key: "key".into(),
        text: "こんにちは".into(),
    });
    model.apply(Update::History {
        companion_id: "ene".to_owned(),
        messages: vec![message("a", 1, MessageRole::User, "こんにちは")],
    });
    assert!(
        model.companions.chat.sending.is_empty(),
        "the host recorded the input"
    );
}

#[test]
fn becoming_live_after_a_drop_refetches_what_the_snapshot_cannot_carry() {
    let mut model = model();
    model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    model.apply(Update::Link(Box::new(Link::Lost {
        reason: "closed".into(),
        retry_in: Duration::from_secs(1),
    })));
    let effects = model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    assert!(effects.history && effects.approvals);
}

#[test]
fn audio_levels_and_transcription_reset_when_link_drops() {
    let mut model = with_chat();
    model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    model.apply(Update::AudioLevel {
        output: 0.4,
        input: 0.2,
        speaking: true,
    });
    model.apply(Update::Transcription {
        text: "こんにちは".into(),
        is_final: false,
        is_speaking: true,
    });
    assert!(model.audio.companion_speaking);
    assert!(model.audio.transcription.is_some());

    model.apply(Update::Link(Box::new(Link::Lost {
        reason: "closed".into(),
        retry_in: Duration::from_secs(1),
    })));
    assert!(!model.audio.companion_speaking);
    assert_eq!(model.audio.output_rms, 0.0);
    assert!(model.audio.transcription.is_none());
}

#[test]
fn mind_updated_requests_refresh_only_for_active_companion() {
    let mut model = with_chat();
    let other = model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::MindUpdated(CompanionId::parse("other").unwrap()),
    ))));
    assert!(!other.mind);

    let mine = model.apply(Update::Notification(Box::new(notification(
        NotificationEvent::MindUpdated(CompanionId::parse("ene").unwrap()),
    ))));
    assert!(mine.mind);
}

#[test]
fn internal_work_messages_do_not_appear_in_chat() {
    let mut model = with_chat();
    let internal = Message {
        id: "w1".into(),
        companion_id: Some(CompanionId::parse("ene").unwrap()),
        work_id: "work-1".into(),
        role: MessageRole::Agent,
        text: "[作業報告 (work_id: work-1)] ...".into(),
        sequence: 1,
        ..Default::default()
    };
    assert!(!is_user_facing_message(&internal));
    model.apply(Update::History {
        companion_id: "ene".to_owned(),
        messages: vec![internal, message("u1", 2, MessageRole::User, "調べて")],
    });
    assert_eq!(model.companions.chat.messages.len(), 1);
    assert_eq!(model.companions.chat.messages[0].text, "調べて");
}

#[test]
fn each_work_clears_only_its_own_activity() {
    let mut model = model();
    let ene = CompanionId::parse("ene").unwrap();
    let mio = CompanionId::parse("mio").unwrap();
    model.snapshot = Some(Snapshot {
        active_work: [("first".into(), ene.clone())].into(),
        ..snapshot()
    });
    let event = |work_id: &str, companion_id, status| {
        Update::Notification(Box::new(notification(
            NotificationEvent::WorkStatusChanged {
                work_id: work_id.into(),
                companion_id,
                status,
            },
        )))
    };
    assert!(model.has_active_work());
    model.apply(event("second", mio.clone(), WorkStatus::Active));
    model.apply(event("first", ene.clone(), WorkStatus::Idle));
    model.apply(event("first", ene, WorkStatus::Idle));
    assert!(model.has_active_work(), "the second work is still running");
    model.apply(event("second", mio, WorkStatus::Closed));
    assert!(!model.has_active_work());
}

#[test]
fn q04_stale_epoch_scoped_updates_are_discarded_after_reconnect() {
    use crate::domain::ModelInfo;

    let mut model = model();
    model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    let first_epoch = model.connections.link_epoch;
    model.apply(Update::Models {
        connection_id: "c1".into(),
        models: vec![ModelInfo {
            id: "m-fresh".into(),
            ..Default::default()
        }],
    });

    model.apply(Update::Link(Box::new(Link::Lost {
        reason: "drop".into(),
        retry_in: Duration::from_secs(1),
    })));
    model.apply(Update::Link(Box::new(Link::Live {
        version: "2".into(),
    })));
    assert!(model.connections.link_epoch > first_epoch);

    model.apply(Update::Scoped {
        epoch: first_epoch,
        seq: 1,
        inner: Box::new(Update::Models {
            connection_id: "c1".into(),
            models: vec![ModelInfo {
                id: "m-stale".into(),
                ..Default::default()
            }],
        }),
    });
    assert_eq!(
        model.connections.models.get("c1").unwrap()[0].id,
        "m-fresh",
        "stale epoch response must not overwrite state after reconnect"
    );
}

#[test]
fn f01_r03_resident_companions_queries_multiple_activations_on_one_client() {
    let mut model = model();
    let mut snap = snapshot();
    snap.activations = vec![
        Activation {
            companion_id: ene_domain::CompanionId::parse("ene").unwrap(),
            client_id: Some("desk-a".into()),
            generation: None,
        },
        Activation {
            companion_id: ene_domain::CompanionId::parse("miku").unwrap(),
            client_id: Some("desk-a".into()),
            generation: None,
        },
        Activation {
            companion_id: ene_domain::CompanionId::parse("kai").unwrap(),
            client_id: Some("desk-b".into()),
            generation: None,
        },
    ];
    model.snapshot = Some(snap);
    assert_eq!(model.resident_companions("desk-a"), vec!["ene", "miku"]);
    assert!(model.is_resident("miku", "desk-a"));
    assert!(!model.is_resident("kai", "desk-a"));
}

fn finalized_line(line: usize) -> Update {
    Update::Transcription {
        text: format!("line-{line}"),
        is_final: true,
        is_speaking: false,
    }
}

#[test]
fn finalized_lines_keep_the_newest_twenty_in_order() {
    let mut model = model();
    model.apply(finalized_line(1));
    assert_eq!(
        model.audio.recognized,
        ["line-1"],
        "a single finalized line must survive"
    );

    for line in 2..=20 {
        model.apply(finalized_line(line));
    }
    assert_eq!(
        model.audio.recognized.len(),
        20,
        "twenty finalized lines all stay"
    );
    assert_eq!(model.audio.recognized[0], "line-1");
    assert_eq!(model.audio.recognized[19], "line-20");

    for line in 21..=25 {
        model.apply(finalized_line(line));
    }
    assert_eq!(
        model.audio.recognized.len(),
        20,
        "the list is capped at twenty"
    );
    assert_eq!(
        model.audio.recognized[0], "line-6",
        "the oldest lines fall off first"
    );
    assert_eq!(
        model.audio.recognized[19], "line-25",
        "the newest line stays last"
    );
}

/// Replacement ordering is local to the resource whose state would change.
#[test]
fn scoped_model_lists_replace_only_the_same_connections_older_reply() {
    let mut model = with_chat();
    model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    let epoch = model.connections.link_epoch;
    let scoped = |seq: u64, connection: &str, id: &str| Update::Scoped {
        epoch,
        seq,
        inner: Box::new(Update::Models {
            connection_id: connection.into(),
            models: vec![crate::domain::ModelInfo {
                id: id.into(),
                name: id.into(),
            }],
        }),
    };

    model.apply(scoped(3, "a", "fresh"));
    model.apply(scoped(1, "b", "first"));
    model.apply(scoped(2, "a", "old"));
    model.apply(scoped(3, "a", "replay"));
    assert_eq!(model.connections.models["a"][0].id, "fresh");
    assert_eq!(model.connections.models["b"][0].id, "first");
    model.apply(scoped(4, "a", "newer"));
    assert_eq!(model.connections.models["a"][0].id, "newer");
}
