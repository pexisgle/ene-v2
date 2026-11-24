use super::*;

#[test]
fn both_roles_follow_the_same_spoken_response_rules() {
    let companion = crate::host::mind::default_ene_companion("");
    for prompt in [
        system_prompt(&companion),
        work_prompt(&companion, "資料をまとめる"),
    ] {
        assert!(prompt.contains(SPOKEN_RESPONSE));
        assert!(!prompt.contains("参考URLも"));
        assert!(!prompt.contains("URLをそのまま添えます"));
    }
}
use rig_core::conversation::{Conversation as History, Entry, RequestId};

#[test]
fn a_tool_called_by_the_head_appears_in_the_activity_feed() {
    use crate::host::tests::{Harness, text_turn};
    use rig_core::test_utils::MockStreamEvent;

    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("observe-tool", "noop", serde_json::json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("続けます"),
    ]);
    let cursor = harness.app.world().resource::<Notifier>().cursor();
    harness.accept("observe", "続けて").unwrap();
    harness.run_until(|world| {
        world.resource::<Notifier>().replay(&cursor).unwrap().iter().any(|event| {
            event.kind == super::super::notify::DIAGNOSTIC
                && matches!(&event.event, Some(w::notification::Event::Generic(inner))
                    if inner.payload.as_option().is_some_and(|p| p.json.contains("\"tool\":\"noop\"")))
        })
    });
}

#[test]
fn transcript_text_keeps_full_text_blocks_without_separators_or_reasoning() {
    use rig_core::completion::{AssistantContent, AssistantMessage};

    let long = "full\ntext ".repeat(30);
    let history = History::from_messages([
        CompletionMessage::User {
            content: vec![UserContent::text("  user"), UserContent::text("\ntext  ")],
        },
        CompletionMessage::Assistant(AssistantMessage::new(vec![
            AssistantContent::text("  first"),
            AssistantContent::reasoning("private reasoning"),
            AssistantContent::text("second\n"),
            AssistantContent::text(long.clone()),
        ])),
        CompletionMessage::Assistant(AssistantMessage::new(vec![
            AssistantContent::reasoning("private reasoning"),
            AssistantContent::text(" \n\t"),
        ])),
        CompletionMessage::system("hidden system context"),
    ]);
    let text: Vec<_> = history.entries().map(|entry| entry_text(&entry)).collect();
    assert_eq!(text[0].as_deref(), Some("user\ntext"));
    assert_eq!(
        text[1],
        Some(format!("firstsecond\n{long}").trim().to_owned())
    );
    assert_eq!(&text[2..], &[None, None]);
}

#[tokio::test]
async fn connected_assignments_observe_cloned_credential_mutations() {
    use crate::host::{model, tests::Harness};
    use rig_core::{
        completion::CompletionRequest,
        effect::{Answered, EffectKind},
        serve::serve_inline,
        test_utils::{MockCompletionModel, MockTurn},
    };
    use std::sync::Arc;

    let mut harness = Harness::new([]);
    let store = harness.store();
    let mut companion = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    companion.assignments.companion = crate::host::mind::Assignment {
        connection_id: "credential-model".into(),
        model_id: "fake".into(),
    };
    store
        .companion_repo()
        .update_companion(&companion, companion.revision)
        .unwrap();
    let credentials = Credentials::new(true);
    let writer = credentials.clone();
    harness.app.world_mut().insert_resource(credentials);
    let spec = rig_core::catalog::Catalog::builtin()
        .resolve("openai/gpt-4o-mini")
        .unwrap()
        .shared();
    harness
        .app
        .world_mut()
        .insert_resource(ModelFactory(Arc::new(move |_, credentials, _, _, _, _| {
            let text = credentials
                .secret("credential-model")
                .ok()
                .flatten()
                .unwrap_or_else(|| "absent".into());
            Ok(
                MockCompletionModel::from_turns((0..8).map(|_| MockTurn::text(text.clone())))
                    .priced_as(spec.clone())
                    .into(),
            )
        })));
    let entity = harness.companion_agent();
    for expected in ["old-key", "new-key", "absent", "direct-key", "absent"] {
        match expected {
            "old-key" | "new-key" => {
                model::save_secret(&writer, "credential-model", expected).unwrap()
            }
            "direct-key" => writer.remember("credential-model", expected.into()),
            _ => model::delete_secret(&writer, "credential-model").unwrap(),
        }
        ensure_model(harness.app.world_mut(), entity).unwrap();
        let handler = harness
            .app
            .world()
            .get::<ModelConnection>(entity)
            .unwrap()
            .handler
            .0
            .clone();
        let answer = serve_inline(
            &handler,
            EffectKind::Completion {
                request: CompletionRequest::new("hi").into(),
                stream: false,
            },
        )
        .await
        .unwrap();
        let Answered::Completion(answer) = answer else {
            panic!("expected completion")
        };
        assert_eq!(answer.text(), expected);
    }
}

#[test]
fn messages_publish_once_oldest_first_across_additions_and_restores() {
    let mut app = App::new();
    app.insert_resource(Notifier::new())
        .add_systems(Update, publish_messages);
    let mut events = app.world().resource::<Notifier>().subscribe();
    let mut history = History::new();
    history.push(Entry::new(CompletionMessage::user("hello")).by(Author {
        request: Some(RequestId("request-1".into())),
        ..Author::user()
    }));
    history.push(CompletionMessage::system("hidden context"));
    history.push(CompletionMessage::assistant("answer"));
    let entity = app
        .world_mut()
        .spawn((
            AgentId("work-run".into()),
            Conversation(history),
            WorkAgent {
                work_id: "work-1".into(),
                run_id: "run-1".into(),
                companion_id: "ene".into(),
            },
        ))
        .id();
    app.update();
    app.world_mut().resource_mut::<Notifier>().flush();
    let messages: Vec<w::Message> = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| match event.event.unwrap() {
            w::notification::Event::MessageCreated(created) => {
                created.message.as_option().unwrap().clone()
            }
            _ => panic!("only transcript messages are published"),
        })
        .collect();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["hello", "answer"]
    );
    assert!(messages[0].sequence < messages[1].sequence);
    assert_eq!(messages[0].request_id.as_deref(), Some("request-1"));
    assert_eq!(messages[0].role, w::MessageRole::User);
    assert_eq!(messages[0].companion_id, "ene");
    assert_eq!(messages[0].work_id, "work-1");
    assert_eq!(messages[0].author, Author::user().label());
    assert_ne!(messages[0].created_at, "");

    let mut restored = app.world().get::<Conversation>(entity).unwrap().0.view();
    restored.push(CompletionMessage::system("more hidden context"));
    app.world_mut()
        .entity_mut(entity)
        .insert(Conversation(restored.view()));
    app.update();
    app.world_mut().resource_mut::<Notifier>().flush();
    assert!(events.try_recv().is_err());

    restored.extend([
        CompletionMessage::user("next"),
        CompletionMessage::assistant("new answer"),
    ]);
    app.world_mut()
        .entity_mut(entity)
        .insert(Conversation(restored));
    app.update();
    app.update();
    app.world_mut().resource_mut::<Notifier>().flush();
    let texts: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| match event.event.unwrap() {
            w::notification::Event::MessageCreated(created) => {
                created.message.as_option().unwrap().text.clone()
            }
            _ => panic!("only transcript messages are published"),
        })
        .collect();
    assert_eq!(texts, ["next", "new answer"]);
}

#[test]
fn turn_events_publish_for_companion_turns_only() {
    use crate::host::tests::{Harness, text_turn};

    let mut harness = Harness::new([text_turn("work answer"), text_turn("companion answer")]);
    let companion = harness
        .store()
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    let work_entity = spawn_work_agent(
        harness.app.world_mut(),
        &companion,
        "work-1",
        "run-1",
        "do task",
    );
    let before_work = harness.app.world().resource::<Notifier>().cursor();
    assert!(deliver(
        harness.app.world_mut(),
        work_entity,
        &harness.companion,
        "start".into(),
        DeliveryMode::Queue,
        Author::user(),
    ));
    harness.run_until(|world| {
        world
            .get::<Conversation>(work_entity)
            .is_some_and(|conversation| {
                conversation
                    .entries()
                    .any(|entry| entry_text(&entry).as_deref() == Some("work answer"))
            })
    });
    harness.tick();

    let work_events = harness
        .app
        .world()
        .resource::<Notifier>()
        .replay(&before_work)
        .expect("work events stay in the replay ring");
    assert!(
        !work_events.iter().any(|event| {
            matches!(
                event.event.as_ref(),
                Some(
                    w::notification::Event::TurnEvent(_) | w::notification::Event::MessageDelta(_)
                )
            )
        }),
        "work agent turns must not emit TurnEvent or MessageDelta: {work_events:?}"
    );
    assert!(
        work_events.iter().any(|event| {
            matches!(
                event.event.as_ref(),
                Some(w::notification::Event::MessageCreated(created))
                    if created.message.as_option().is_some_and(|message| {
                        message.companion_id == harness.companion && message.work_id == "work-1"
                    })
            )
        }),
        "work agent transcript messages still carry companion_id and work_id: {work_events:?}"
    );

    let before_chat = harness.app.world().resource::<Notifier>().cursor();
    harness.accept("k1", "hello").unwrap();
    let head_entity = harness.companion_agent();
    harness.run_until(|world| {
        world
            .get::<Conversation>(head_entity)
            .is_some_and(|conversation| {
                conversation
                    .entries()
                    .any(|entry| entry_text(&entry).as_deref() == Some("companion answer"))
            })
    });
    harness.tick();

    let chat_events = harness
        .app
        .world()
        .resource::<Notifier>()
        .replay(&before_chat)
        .expect("chat events stay in the replay ring");
    let turns: Vec<_> = chat_events
        .iter()
        .filter_map(|event| match event.event.as_ref()? {
            w::notification::Event::TurnEvent(turn) => Some((event.companion_id.as_deref(), turn)),
            _ => None,
        })
        .collect();
    assert_eq!(turns.len(), 2, "expected Streaming and Answered: {turns:?}");
    assert_eq!(turns[0].1.status, w::TurnStatus::Streaming);
    assert_eq!(turns[1].1.status, w::TurnStatus::Answered);
    for (notification_companion, turn) in turns {
        assert_eq!(notification_companion, Some(harness.companion.as_str()));
        assert_eq!(turn.companion_id, harness.companion);
        assert_eq!(turn.turn_id, "0");
    }
}
