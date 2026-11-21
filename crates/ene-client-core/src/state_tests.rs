use crate::{
    reducer::is_user_facing_message,
    state::{Link, Model, Sending},
    update::Update,
};
use ene_rpc as w;
use std::time::Duration;

fn model() -> Model {
    Model::new("connection.json".into())
}

fn snapshot() -> w::Snapshot {
    w::Snapshot {
        server_version: "1".into(),
        pending_approvals: Vec::new(),
        ..Default::default()
    }
}

fn notification(event: w::notification::Event) -> w::Notification {
    w::Notification {
        kind: "test".into(),
        event: Some(event),
        ..Default::default()
    }
}

fn message(id: &str, sequence: u64, role: w::MessageRole, text: &str) -> w::Message {
    w::Message {
        id: id.into(),
        companion_id: "ene".into(),
        role: role.into(),
        text: text.into(),
        sequence,
        ..Default::default()
    }
}

fn with_chat() -> Model {
    let mut model = model();
    model.companions.chat.companion_id = "ene".into();
    model
}

#[test]
fn a_delta_is_a_preview_until_the_host_records_the_message() {
    let mut model = with_chat();
    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
            companion_id: "ene".into(),
            stream_id: "stream".into(),
            delta: "こん".into(),
            ..Default::default()
        })),
    ))));
    assert!(
        model.companions.chat.messages.is_empty(),
        "a delta is not a message"
    );
    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
            companion_id: "ene".into(),
            stream_id: "stream".into(),
            delta: "にちは".into(),
            ..Default::default()
        })),
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
        w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
            message: Some(message("a", 9, w::MessageRole::Assistant, "こんにちは")).into(),
            ..Default::default()
        })),
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
        Update::Notification(Box::new(notification(
            w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                message: Some(message(id, sequence, w::MessageRole::User, id)).into(),
                ..Default::default()
            })),
        )))
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
        w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
            companion_id: "other".into(),
            turn_id: "1".into(),
            status: w::TurnStatus::Failed.into(),
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
        w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
            companion_id: "ene".into(),
            turn_id: "1".into(),
            status: w::TurnStatus::Stopped.into(),
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
    let pending = |status: w::ApprovalStatus| w::ApprovalRequest {
        request_id: "r1".into(),
        status: status.into(),
        ..Default::default()
    };
    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
            request: Some(pending(w::ApprovalStatus::Pending)).into(),
            ..Default::default()
        })),
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
    let entry = w::PolicyText {
        id: "pt-1".into(),
        text: "外部送信は毎回確認".into(),
        ..Default::default()
    };
    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::PolicyTextsUpdated(Box::new(w::PolicyTextsUpdated {
            texts: vec![entry],
            ..Default::default()
        })),
    ))));
    assert_eq!(model.policy_texts().len(), 1);
    assert_eq!(model.policy_texts()[0].text, "外部送信は毎回確認");
}

#[test]
fn an_older_companion_revision_never_replaces_a_newer_one() {
    let mut model = model();
    model.snapshot = Some(snapshot());
    let companion = |revision: u64, name: &str| w::Companion {
        id: "ene".into(),
        name: name.into(),
        revision,
        ..Default::default()
    };
    let update = |revision: u64, name: &str| {
        Update::Notification(Box::new(notification(w::notification::Event::Companion(
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
    let settings = |revision: u64| w::Settings {
        revision,
        ..Default::default()
    };
    let update = |revision: u64| {
        Update::Notification(Box::new(notification(
            w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
                settings: Some(settings(revision)).into(),
                ..Default::default()
            })),
        )))
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
    assert!(model.companions.chat.sending.is_empty());
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
    assert!(model.companions.chat.sending.is_empty());
}

#[test]
fn a_recorded_user_message_closes_its_pending_row() {
    let mut model = with_chat();
    model.companions.chat.sending.push(Sending {
        request_key: "key".into(),
        text: "こんにちは".into(),
    });
    model.apply(Update::History {
        companion_id: "ene".into(),
        messages: vec![message("a", 1, w::MessageRole::User, "こんにちは")],
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
        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
            companion_id: "other".into(),
            ..Default::default()
        })),
    ))));
    assert!(!other.mind);

    let mine = model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
            companion_id: "ene".into(),
            ..Default::default()
        })),
    ))));
    assert!(mine.mind);
}

#[test]
fn internal_work_messages_are_hidden_and_work_event_tracks_active_status() {
    let mut model = with_chat();
    let internal = w::Message {
        id: "w1".into(),
        companion_id: "ene".into(),
        work_id: "work-1".into(),
        role: w::MessageRole::Agent.into(),
        text: "[作業報告 (work_id: work-1)] ...".into(),
        sequence: 1,
        ..Default::default()
    };
    assert!(!is_user_facing_message(&internal));
    model.apply(Update::History {
        companion_id: "ene".into(),
        messages: vec![internal, message("u1", 2, w::MessageRole::User, "調べて")],
    });
    assert_eq!(model.companions.chat.messages.len(), 1);
    assert_eq!(model.companions.chat.messages[0].text, "調べて");

    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
            work_id: "work-1".into(),
            run_id: "run-1".into(),
            kind: w::WorkEventKind::RunStarted.into(),
            detail: Some(w::work_event::Detail::RunStarted(Box::default())),
            ..Default::default()
        })),
    ))));
    assert!(model.companions.work_active);

    model.apply(Update::Notification(Box::new(notification(
        w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
            work_id: "work-1".into(),
            run_id: "run-1".into(),
            kind: w::WorkEventKind::RunFinished.into(),
            detail: Some(w::work_event::Detail::RunFinished(Box::new(
                w::WorkRunFinished {
                    state: w::RunStatus::Completed.into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })),
    ))));
    assert!(!model.companions.work_active);
}

#[test]
fn q04_stale_epoch_scoped_updates_are_discarded_after_reconnect() {
    let mut model = model();
    model.apply(Update::Link(Box::new(Link::Live {
        version: "1".into(),
    })));
    let first_epoch = model.connections.link_epoch;
    model.apply(Update::Models {
        connection_id: "c1".into(),
        models: vec![w::Model {
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
            models: vec![w::Model {
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
        w::Activation {
            companion_id: "ene".into(),
            client_id: Some("desk-a".into()),
            ..Default::default()
        },
        w::Activation {
            companion_id: "miku".into(),
            client_id: Some("desk-a".into()),
            ..Default::default()
        },
        w::Activation {
            companion_id: "kai".into(),
            client_id: Some("desk-b".into()),
            ..Default::default()
        },
    ];
    model.snapshot = Some(snap);
    assert_eq!(model.resident_companions("desk-a"), vec!["ene", "miku"]);
    assert!(model.is_resident("miku", "desk-a"));
    assert!(!model.is_resident("kai", "desk-a"));
}
