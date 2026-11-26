use super::*;
use crate::host::{
    client,
    mind::ModelAssignment,
    tests::{Harness, bound, text_turn},
};
use rig_core::{
    catalog::Catalog,
    test_utils::{MockCompletionModel, MockStreamEvent, MockTurn},
};
use std::sync::Arc;
use w::perception_event::Event;

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode(
            &vec![32; (width * height) as usize],
            width,
            height,
            image::ExtendedColorType::L8,
        )
        .unwrap();
    bytes
}

fn bind(
    harness: &mut Harness,
    client_id: &str,
) -> (String, tokio::sync::mpsc::Receiver<w::DeviceEvent>) {
    let world = harness.app.world_mut();
    client::register(
        world,
        &w::RegisterClientRequest {
            id: client_id.into(),
            capabilities: vec!["eye.watch".into()],
            capability_specs: vec![w::ClientCapabilitySpec {
                name: "eye.watch".into(),
                input_schema: r#"{"type":"object"}"#.into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .unwrap();
    client::activate(
        world,
        &w::ActivationRequest {
            client_id: client_id.into(),
            companion_id: harness.companion.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    world.resource::<DeviceHub>().register(client_id)
}

fn report(harness: &mut Harness, client: &str, subscription: &str, event: Event) {
    client::report_perception(
        harness.app.world_mut(),
        &w::PerceptionEvent {
            client_id: client.into(),
            subscription_id: subscription.into(),
            event: Some(event),
            ..Default::default()
        },
    )
    .unwrap();
}

fn ready_to_evaluate(harness: &mut Harness) {
    harness
        .app
        .world_mut()
        .resource_mut::<Eyes>()
        .sessions
        .get_mut(&harness.companion)
        .unwrap()
        .next_evaluation = Instant::now();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_head_can_stay_silent_on_routine_reports_and_answer_the_important_fact() {
    let silent_turn = |id| {
        vec![
            MockStreamEvent::tool_call(id, "noop", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    };
    let (mut harness, eye, head) = setup_eye_and_head_harness(
        [
            MockTurn::text(r#"{"notify":false,"summary":"操作中"}"#),
            MockTurn::text(r#"{"notify":true,"summary":"エディターに切り替えた"}"#),
            MockTurn::text(
                r#"{"notify":true,"summary":"routine-window-operations","important":{"kind":"response_check"}}"#,
            ),
            MockTurn::text(
                r#"{"notify":false,"summary":"","important":{"kind":"response_check"}}"#,
            ),
        ],
        [
            silent_turn("quiet-first"),
            text_turn("見えてるよ。"),
            silent_turn("quiet-second"),
            text_turn("監視対象が開いたよ"),
        ],
    );
    let (sub, _stream) = bind(&mut harness, "desktop");
    let mut notifications = harness.app.world().resource::<Notifier>().subscribe();
    report(&mut harness, "desktop", &sub, Event::Enabled(true));
    assert_initial_image_assessment(&mut harness, &sub, &eye, &head);
    assert_head_silence_and_redaction(&mut harness, &sub, &head);
    assert_response_check_and_watch_turns(&mut harness, &sub, &eye, &head);
    let frame_id =
        assert_eye_notifications_and_dispositions(&mut harness, &mut notifications, &head);
    report(&mut harness, "desktop", &sub, Event::Enabled(false));
    assert!(
        preview(
            harness.app.world().resource::<Eyes>(),
            harness.app.world().resource::<DeviceHub>(),
            &frame_id
        )
        .is_err()
    );
    assert_eq!(eye.requests().len(), 4);
}

fn setup_eye_and_head_harness(
    eye_turns: impl IntoIterator<Item = MockTurn>,
    head_turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
) -> (Harness, MockCompletionModel, MockCompletionModel) {
    let mut harness = Harness::new([]);
    let store = harness.store();
    let mut companion = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    companion.assignments.companion = ModelAssignment {
        connection_id: "head".into(),
        model_id: "shared".into(),
    };
    store
        .companion_repo()
        .update_companion(&companion, companion.revision)
        .unwrap();
    let eye = MockCompletionModel::from_turns(eye_turns).priced_as(
        Catalog::builtin()
            .resolve("openai/gpt-4o-mini")
            .unwrap()
            .shared(),
    );
    let head = MockCompletionModel::from_stream_turns(head_turns);
    let factory_eye = eye.clone();
    let factory_head = head.clone();
    harness
        .app
        .insert_resource(HostRuntime(tokio::runtime::Handle::current()))
        .insert_resource(ModelFactory(Arc::new(
            move |_, _, _, companion, role, _| {
                Ok(if role == Role::Perception {
                    assert_eq!(companion.model(role).unwrap().model_id, "shared");
                    factory_eye.clone().into()
                } else {
                    bound(&factory_head)
                })
            },
        )));
    (harness, eye, head)
}

fn assert_initial_image_assessment(
    harness: &mut Harness,
    sub: &str,
    eye: &MockCompletionModel,
    head: &MockCompletionModel,
) {
    report(
        harness,
        "desktop",
        sub,
        Event::ScreenFrame(Box::new(w::ScreenFrame {
            jpeg: jpeg(8, 8),
            ..Default::default()
        })),
    );
    ready_to_evaluate(harness);
    let id = harness.companion.clone();
    harness.run_until(|world| world.resource::<Eyes>().sessions[&id].previous == "操作中");
    assert_eq!(
        head.requests(),
        [] as [rig_core::completion::CompletionRequest; 0]
    );
    let request = &eye.requests()[0];
    assert_eq!(
        request.tools,
        [] as [std::sync::Arc<rig_core::completion::ToolDefinition>; 0]
    );
    assert!(request.chat_history.iter().any(|message| {
        matches!(message.as_ref(), Message::User { content } if content.iter().any(|item| matches!(item, UserContent::Image(_))))
    }));
}

fn assert_head_silence_and_redaction(harness: &mut Harness, sub: &str, head: &MockCompletionModel) {
    let id = harness.companion.clone();
    report(
        harness,
        "desktop",
        sub,
        Event::ActiveWindow(Box::new(w::ActiveWindowChanged {
            title: "synthetic-window".into(),
            ..Default::default()
        })),
    );
    ready_to_evaluate(harness);
    let entity = harness.companion_agent();
    harness
        .run_until(|world| head.requests().len() == 1 && world.get::<ActiveTurn>(entity).is_none());
    let requests = head.requests();
    assert_eq!(requests.len(), 1);
    let request = serde_json::to_string(&requests[0]).unwrap();
    assert!(request.contains("エディターに切り替えた"));
    assert!(!request.contains("synthetic-window"));
    assert!(!harness.conversation(entity).entries().any(|entry| {
        matches!(entry.message, Message::Assistant(_)) && agent::entry_text(&entry).is_some()
    }));
    let path = harness
        .root
        .path()
        .join("rig/logs")
        .join(format!("{}.jsonl", agent::companion_agent_id(&id)));
    let log = std::fs::read_to_string(path).unwrap();
    assert!(!log.contains("エディターに切り替えた"));
    assert!(!log.contains("synthetic-window"));
}

fn assert_response_check_and_watch_turns(
    harness: &mut Harness,
    sub: &str,
    eye: &MockCompletionModel,
    head: &MockCompletionModel,
) {
    let id = harness.companion.clone();
    let entity = harness.companion_agent();
    client::report_perception(
        harness.app.world_mut(),
        &w::PerceptionEvent {
            client_id: "desktop".into(),
            event: Some(Event::PerceptionSummary(Box::new(w::PerceptionSummary {
                summary: "unfiltered-passive-window-log".into(),
                ..Default::default()
            }))),
            ..Default::default()
        },
    )
    .unwrap();
    let screenshot = || {
        Event::ScreenFrame(Box::new(w::ScreenFrame {
            jpeg: jpeg(8, 8),
            ..Default::default()
        }))
    };
    report(harness, "desktop", sub, screenshot());
    report(
        harness,
        "desktop",
        sub,
        Event::ActiveWindow(Box::new(w::ActiveWindowChanged {
            title: "foreground-changed-after-capture".into(),
            ..Default::default()
        })),
    );
    ready_to_evaluate(harness);
    harness
        .run_until(|world| head.requests().len() == 2 && world.get::<ActiveTurn>(entity).is_none());
    let request = serde_json::to_string(&head.requests()[1]).unwrap();
    assert!(request.contains("見えているかの確認として「応答して」"));
    assert!(!request.contains("routine-window-operations"));
    assert!(!request.contains("foreground-changed-after-capture"));
    assert!(!request.contains("unfiltered-passive-window-log"));
    report(harness, "desktop", sub, screenshot());
    ready_to_evaluate(harness);
    harness.run_until(|world| {
        eye.requests().len() == 4 && world.resource::<Eyes>().sessions[&id].task.is_none()
    });
    assert_eq!(head.requests().len(), 2);

    let watch = |title: &str| {
        Event::WatchMatch(Box::new(w::WatchMatch {
            window: Some(w::ActiveWindowChanged {
                title: title.into(),
                ..Default::default()
            })
            .into(),
            transition: w::WatchTransition::Focus.into(),
            ..Default::default()
        }))
    };
    report(harness, "desktop", sub, watch("quiet-watch"));
    harness
        .run_until(|world| head.requests().len() == 3 && world.get::<ActiveTurn>(entity).is_none());
    report(harness, "desktop", sub, watch("watch-target"));
    harness
        .run_until(|world| head.requests().len() == 4 && world.get::<ActiveTurn>(entity).is_none());
    let requests = head.requests();
    assert!(
        serde_json::to_string(&requests[2])
            .unwrap()
            .contains("quiet-watch")
    );
    assert!(
        serde_json::to_string(&requests[3])
            .unwrap()
            .contains("watch-target")
    );
}

fn assert_eye_notifications_and_dispositions(
    harness: &mut Harness,
    notifications: &mut tokio::sync::broadcast::Receiver<w::Notification>,
    head: &MockCompletionModel,
) -> String {
    let _discarded = head;
    let entity = harness.companion_agent();
    let mut delivered = Vec::new();
    let mut important = Vec::new();
    let mut results = Vec::new();
    let mut received_frame = None;
    let mut evaluated_frame = None;
    while let Ok(notification) = notifications.try_recv() {
        match notification.event {
            Some(w::notification::Event::TurnEvent(turn)) => {
                if let Some(result) = turn.eye_report.as_option() {
                    results.push((
                        result.report_id.clone(),
                        result.disposition.as_known().unwrap(),
                    ));
                }
            }
            Some(w::notification::Event::Generic(event)) if event.kind == "diagnostic" => {
                let data: Value =
                    serde_json::from_str(&event.payload.as_option().unwrap().json).unwrap();
                let detail = &data["detail"];
                match data["action"].as_str() {
                    Some("delivered") => {
                        delivered.push(detail["report_id"].as_str().unwrap().to_owned());
                        important.push(detail["important"].as_bool().unwrap());
                    }
                    Some("screen_received") => {
                        received_frame = detail["frame_id"].as_str().map(str::to_owned)
                    }
                    Some("evaluated") if detail["input"] == "image" => {
                        evaluated_frame = detail["frame_id"].as_str().map(str::to_owned);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    use w::eye_report_result::Disposition;
    let frame_id = received_frame.unwrap();
    assert_eq!(evaluated_frame.as_deref(), Some(frame_id.as_str()));
    assert_eq!(
        preview(
            harness.app.world().resource::<Eyes>(),
            harness.app.world().resource::<DeviceHub>(),
            &frame_id
        )
        .unwrap()
        .jpeg,
        jpeg(8, 8)
    );
    assert_eq!(delivered.len(), 4);
    assert_eq!(important, [false, true, false, false]);
    assert_eq!(
        results,
        vec![
            (delivered[0].clone(), Disposition::Watched),
            (delivered[1].clone(), Disposition::Replied),
            (delivered[2].clone(), Disposition::Watched),
            (delivered[3].clone(), Disposition::Replied),
        ]
    );
    assert_eq!(
        harness
            .conversation(entity)
            .entries()
            .filter(|entry| {
                matches!(entry.message, Message::Assistant(_)) && agent::entry_text(entry).is_some()
            })
            .count(),
        2
    );
    frame_id
}

#[test]
fn previews_keep_recent_frames_after_new_captures() {
    let mut harness = Harness::new([]);
    let (sub, _stream) = bind(&mut harness, "desktop");
    report(&mut harness, "desktop", &sub, Event::Enabled(true));
    let companion = harness.companion.clone();
    let frames: Vec<_> = (0..=PREVIEW_FRAMES)
        .map(|index| {
            let jpeg = jpeg(8 + index as u32, 8);
            report(
                &mut harness,
                "desktop",
                &sub,
                Event::ScreenFrame(Box::new(w::ScreenFrame {
                    jpeg: jpeg.clone(),
                    ..Default::default()
                })),
            );
            let session = &harness.app.world().resource::<Eyes>().sessions[&companion];
            (session.recent_screens.back().unwrap().id.clone(), jpeg)
        })
        .collect();
    assert!(
        preview(
            harness.app.world().resource::<Eyes>(),
            harness.app.world().resource::<DeviceHub>(),
            &frames[0].0
        )
        .is_err()
    );
    for (id, jpeg) in &frames[1..] {
        assert_eq!(
            preview(
                harness.app.world().resource::<Eyes>(),
                harness.app.world().resource::<DeviceHub>(),
                id
            )
            .unwrap()
            .jpeg,
            *jpeg
        );
    }
    harness
        .app
        .world_mut()
        .resource_mut::<Eyes>()
        .sessions
        .get_mut(&companion)
        .unwrap()
        .recent_screens
        .front_mut()
        .unwrap()
        .at = Instant::now() - PREVIEW_LIFETIME - Duration::from_secs(1);
    assert!(
        preview(
            harness.app.world().resource::<Eyes>(),
            harness.app.world().resource::<DeviceHub>(),
            &frames[1].0
        )
        .is_err()
    );
    assert!(
        preview(
            harness.app.world().resource::<Eyes>(),
            harness.app.world().resource::<DeviceHub>(),
            &frames[2].0
        )
        .is_ok()
    );
}

#[test]
fn disable_and_handoff_clear_a_watch_waiting_to_wake_the_head() {
    let mut harness = Harness::new([]);
    let (sub, _stream) = bind(&mut harness, "desktop");
    let watch = || {
        Event::WatchMatch(Box::new(w::WatchMatch {
            transition: w::WatchTransition::Blur.into(),
            window: Some(w::ActiveWindowChanged {
                title: "watch-target".into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        }))
    };
    let id = harness.companion.clone();
    report(&mut harness, "desktop", &sub, watch());
    assert!(harness.app.world().resource::<Eyes>().sessions.is_empty());
    report(&mut harness, "desktop", &sub, Event::Enabled(true));
    report(&mut harness, "desktop", &sub, watch());
    assert!(
        harness.app.world().resource::<Eyes>().sessions[&id]
            .pending
            .as_ref()
            .unwrap()
            .kind
            == ReportKind::Watch
    );
    report(&mut harness, "desktop", &sub, Event::Enabled(false));
    report(&mut harness, "desktop", &sub, watch());
    assert!(harness.app.world().resource::<Eyes>().sessions.is_empty());
    report(&mut harness, "desktop", &sub, Event::Enabled(true));
    report(&mut harness, "desktop", &sub, watch());
    let (_new_sub, _new_stream) = bind(&mut harness, "new-desktop");
    report(&mut harness, "desktop", &sub, Event::Enabled(true));
    assert!(harness.app.world().resource::<Eyes>().sessions.is_empty());
}

#[test]
fn invalid_frames_and_repeated_or_malformed_assessments_are_rejected() {
    assert!(!valid_jpeg(b"not a JPEG"));
    assert!(!valid_jpeg(&jpeg(w::MAX_PERCEPTION_FRAME_EDGE + 1, 1)));
    assert!(valid_jpeg(&jpeg(8, 8)));
    assert!(parse_assessment(r#"{"notify":true,"summary":""}"#).is_none());
    let now = Instant::now();
    let mut session = Session::new("client".into(), "subscription".into(), now);
    session.last_automatic = Some((now, "first".into()));
    session.queue_assessment(
        now,
        Assessment {
            notify: true,
            summary: "second".into(),
            important: None,
        },
        false,
    );
    assert!(session.pending.is_none());
    session.queue_assessment(
        now + REACTION_INTERVAL,
        Assessment {
            notify: true,
            summary: "first".into(),
            important: None,
        },
        false,
    );
    assert!(session.pending.is_none());
    assert_eq!(
        session.queue_assessment(
            now,
            parse_assessment(
                r#"{"notify":false,"summary":"","important":{"kind":"response_check"}}"#
            )
            .unwrap(),
            false
        ),
        "not_important"
    );
    assert!(session.pending_important.is_empty());
    assert_eq!(session.queue_assessment(now, parse_assessment(r#"{"notify":false,"summary":"routine","important":{"kind":"finding","summary":"ビルドが失敗"}}"#).unwrap(), false), "important_queued");
    assert_eq!(
        session.pending_important.front().unwrap().summary,
        "ビルドが失敗"
    );
}

#[test]
fn quiet_heartbeat_delivers_no_change_report_after_one_minute() {
    let now = Instant::now();
    let mut session = Session::new("client".into(), "subscription".into(), now);
    // Before 1 minute has elapsed, notify=false stays suppressed.
    assert_eq!(
        session.queue_assessment(
            now + Duration::from_secs(30),
            Assessment {
                notify: false,
                summary: "静かな作業中".into(),
                important: None,
            },
            false,
        ),
        "not_important"
    );
    assert!(session.pending.is_none());

    // Once QUIET_HEARTBEAT_INTERVAL (60s) has elapsed, even a no-change assessment
    // queues a periodic Heartbeat report so the head can react with idle motions.
    assert_eq!(
        session.queue_assessment(
            now + QUIET_HEARTBEAT_INTERVAL,
            Assessment {
                notify: false,
                summary: String::new(),
                important: None,
            },
            false,
        ),
        "heartbeat_queued"
    );
    let pending = session.pending.as_ref().expect("heartbeat queued");
    assert!(pending.kind == ReportKind::Heartbeat);
    assert!(pending.summary.contains("特に目立った変化はありません"));
}
