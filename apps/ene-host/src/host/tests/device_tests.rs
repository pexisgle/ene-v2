use super::*;
use crate::host::{
    agent, chat, client,
    client::device,
    db::Store,
    permission,
    resources::{DeviceHub, Waiting},
    work,
};
use ene_rpc as w;
use rig_core::catalog::Catalog;
use rig_core::completion::Message;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_ecs::prelude::*;
use std::{sync::Arc, time::Duration};

#[test]
fn a_shared_capability_tool_serves_each_companion_that_declares_it() {
    let mut harness = Harness::new([
        tool_turn(
            "call-a",
            "body_motion",
            serde_json::json!({"motion": "nod"}),
        ),
        text_turn("A done"),
        tool_turn(
            "call-b",
            "body_motion",
            serde_json::json!({"motion": "wave"}),
        ),
        text_turn("B done"),
    ]);
    let other = setup_shared_body_motion_companions(&mut harness);
    assert_shared_body_motion_visibility(&mut harness, &other);
    assert_shared_body_motion_dispatch(&mut harness, &other);
    assert_hand_never_sees_capability_tools(&mut harness);
}

fn setup_shared_body_motion_companions(harness: &mut Harness) -> String {
    let other = crate::host::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "other".into(),
            mind: "kind".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    client::declare(
        harness.app.world_mut(),
        client::Declaration::Register(&w::RegisterClientRequest {
            id: "c1".into(),
            name: "c1".into(),
            platform: "linux".into(),
            capabilities: vec!["body.motion".into(), "app.focus".into()],
            capability_specs: vec![
                capability_tool("body.motion", "body_motion"),
                capability_tool("app.focus", "focus_app"),
            ],
            ..Default::default()
        }),
    )
    .unwrap();
    register(
        harness.app.world_mut(),
        "c2",
        capability_tool("body.motion", "body_motion"),
    );
    activate(harness.app.world_mut(), &harness.companion, "c1");
    activate(harness.app.world_mut(), &other, "c2");
    other
}

fn assert_shared_body_motion_visibility(harness: &mut Harness, other: &str) {
    // One model-facing definition, one binding per active companion client.
    let bindings = published_bindings(harness.app.world_mut(), "body_motion");
    assert_eq!(bindings.len(), 2);
    for (companion, client) in [(&harness.companion, "c1"), (&other.to_owned(), "c2")] {
        assert!(
            bindings.iter().any(|binding| {
                binding.companion_id == *companion && binding.client_id == client
            })
        );
    }
    // Both parents see the shared name; each sees only its own companion's
    // capability tools.
    let access_a = access_of(harness.app.world_mut(), &harness.companion);
    assert!(access_a.allows("body_motion"));
    assert!(access_a.allows("focus_app"));
    let access_b = access_of(harness.app.world_mut(), other);
    assert!(access_b.allows("body_motion"));
    assert!(
        !access_b.allows("focus_app"),
        "another companion's capability tool must not be offered"
    );
    assert_eq!(
        published_bindings(harness.app.world_mut(), "focus_app").len(),
        1
    );
}

fn assert_shared_body_motion_dispatch(harness: &mut Harness, other: &str) {
    // A's call is dispatched to A's active client.
    harness.accept("key-a", "A please").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    harness.run_until(|world| said(world, "A done"));
    assert!(
        harness
            .store()
            .client_repo()
            .outstanding_operations("c2")
            .unwrap()
            .is_empty(),
        "A's call must not reach B's client"
    );
    // B's call of the same tool name is dispatched to B's active client.
    chat::accept(
        &mut harness.app,
        &w::SendMessageRequest {
            companion_id: other.to_owned(),
            request_key: "key-b".into(),
            text: "B please".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c2")
            .unwrap()
            .is_empty()
    });
    harness.run_until(|world| said(world, "B done"));
    let a_operations = harness
        .store()
        .client_repo()
        .outstanding_operations("c1")
        .unwrap();
    assert_eq!(a_operations.len(), 1);
    assert_eq!(a_operations[0].capability, "body.motion");
    assert_eq!(a_operations[0].client_id, "c1");
    assert_eq!(a_operations[0].input, serde_json::json!({"motion": "nod"}));
    let b_operations = harness
        .store()
        .client_repo()
        .outstanding_operations("c2")
        .unwrap();
    assert_eq!(b_operations.len(), 1);
    assert_eq!(b_operations[0].capability, "body.motion");
    assert_eq!(b_operations[0].client_id, "c2");
    assert_eq!(b_operations[0].input, serde_json::json!({"motion": "wave"}));
}

fn assert_hand_never_sees_capability_tools(harness: &mut Harness) {
    let companion = harness
        .store()
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    let hand = agent::spawn_work_agent(
        harness.app.world_mut(),
        &companion,
        "work-1",
        "run-1",
        "do the task",
    );
    client::dynamic::sync_tools(harness.app.world_mut()).unwrap();
    let access = harness.app.world().get::<ToolAccess>(hand).unwrap();
    assert!(!access.allows("body_motion"));
    assert!(access.allows("report") && access.allows("device_request"));
}

#[test]
fn a_shared_tool_name_with_different_declarations_is_a_conflict() {
    let mut harness = Harness::new([]);
    let other = crate::host::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "other".into(),
            mind: "kind".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    register(
        harness.app.world_mut(),
        "c1",
        capability_tool("body.motion", "body_motion"),
    );
    register(
        harness.app.world_mut(),
        "c2",
        w::ClientCapabilitySpec {
            tool_name: "body_motion".into(),
            input_schema: r#"{"type":"object","required":["motion"],
                 "properties":{"motion":{"type":"string"}}}"#
                .into(),
            approval: w::CapabilityApproval::Auto.into(),
            ..spec("body.motion")
        },
    );
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let error = client::activate(
        harness.app.world_mut(),
        &w::ActivationRequest {
            companion_id: other,
            client_id: "c2".into(),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        crate::host::error::HostError::Conflict(ref reason)
            if reason.contains("different description or input schema")
    ));
}

#[test]
fn only_device_request_waits_for_the_client_result() {
    let mut harness = Harness::new([
        tool_turn(
            "call-1",
            "body_motion",
            serde_json::json!({"motion": "nod"}),
        ),
        text_turn("first"),
        tool_turn(
            "call-2",
            "device_request",
            serde_json::json!({"capability": "body.motion", "input": {"motion": "nod"}}),
        ),
        text_turn("second"),
    ]);
    register(
        harness.app.world_mut(),
        "c1",
        capability_tool("body.motion", "body_motion"),
    );
    activate(harness.app.world_mut(), &harness.companion, "c1");
    // Keep the result-waiting deadline out of the assertion window.
    harness
        .app
        .world_mut()
        .resource_mut::<crate::host::resources::DeviceTimeout>()
        .0 = Duration::from_secs(30);
    // A named capability tool answers immediately: the operation exists but
    // no tool call waits for its result.
    harness.accept("key-1", "move").unwrap();
    harness.run_until(|world| {
        world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .len()
            == 1
    });
    assert!(
        harness
            .app
            .world()
            .resource::<Waiting>()
            .by_operation
            .is_empty(),
        "a named capability tool must not wait for the device result"
    );
    harness.run_until(|world| said(world, "first"));
    // device_request keeps its Waiting entry until the client answers.
    harness.accept("key-2", "wait").unwrap();
    harness.run_until(|world| {
        world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .len()
            == 2
    });
    let operations = harness
        .store()
        .client_repo()
        .outstanding_operations("c1")
        .unwrap();
    let waiting = &harness.app.world().resource::<Waiting>().by_operation;
    assert_eq!(waiting.len(), 1);
    assert!(
        operations
            .iter()
            .any(|operation| waiting.contains_key(&operation.id)),
        "the waiting call belongs to the device_request operation"
    );
}

pub(super) fn start_work(harness: &mut Harness) -> w::WorkSession {
    work::start(
        harness.app.world_mut(),
        &w::StartWorkRequest {
            companion_id: harness.companion.clone(),
            title: "task".into(),
            instruction: "list the folder".into(),
            ..Default::default()
        },
    )
    .unwrap()
}

fn computer_model(
    harness: &mut Harness,
    turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
    image: bool,
) -> MockCompletionModel {
    let model = MockCompletionModel::from_stream_turns(turns);
    let mut facts = (*Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .shared())
    .clone();
    facts.input.image = image;
    let bound = model.clone().priced_as(Arc::new(facts));
    harness
        .app
        .world_mut()
        .insert_resource(crate::host::model::ModelFactory(Arc::new(
            move |_, _, _, _, _, _| Ok(bound.clone().into()),
        )));
    harness
        .app
        .world_mut()
        .resource_mut::<crate::host::resources::DeviceTimeout>()
        .0 = Duration::from_secs(5);
    model
}

#[test]
fn computer_use_sends_a_request_only_image_and_never_replays_input() {
    use serde_json::json;

    let mut harness = Harness::new([]);
    let frame_id = uuid::Uuid::new_v4().to_string();
    let model = setup_computer_use_model(&mut harness, &frame_id);
    let hub = harness.app.world().resource::<DeviceHub>().clone();
    let (_, mut stream) = hub.register(&ene_domain::ClientId::parse("desk").unwrap());
    let mut events = harness
        .app
        .world()
        .resource::<crate::host::notify::Notifier>()
        .subscribe();
    let session = start_work(&mut harness);
    let run = harness
        .store()
        .work_repo()
        .active_run(&session.id)
        .unwrap()
        .unwrap()
        .run_id;
    wait_for_desk_operation(&mut harness);
    let Some(w::device_event::Event::Operation(shot)) = stream.try_recv().unwrap().event else {
        panic!("the screen request was not dispatched");
    };
    assert_eq!(shot.run_id.as_deref(), Some(run.as_str()));
    assert_eq!(shot.work_id.as_deref(), Some(session.id.as_str()));
    let (result, encoded) = complete_screenshot_op(&mut harness, &shot.operation_id, &frame_id);
    wait_for_desk_operation(&mut harness);
    let Some(w::device_event::Event::Operation(click)) = stream.try_recv().unwrap().event else {
        panic!("the click was not dispatched");
    };
    assert_eq!(click.run_id.as_deref(), Some(run.as_str()));
    assert_redacted_screenshot_effects(
        &harness,
        &model,
        &mut events,
        &ScreenshotAudit {
            operation_id: &result.operation_id,
            run: &run,
            frame_id: &frame_id,
            encoded: &encoded,
        },
    );
    work::stop(harness.app.world_mut(), &session.id).unwrap();
    let (_, mut replacement) = hub.register(&ene_domain::ClientId::parse("desk").unwrap());
    crate::host::client::computer::invalidate_client(
        harness.app.world_mut(),
        "desk",
        hub.subscription("desk"),
    );
    let context = device::DeviceDispatchContext {
        store: &harness.store(),
        hub: &hub,
        notifier: harness.app.world().resource(),
    };
    device::resend(&context, "desk", |_| {
        panic!("stopped Computer Use operations are already terminal")
    })
    .unwrap();
    assert!(replacement.try_recv().is_err());
    let late = device::complete(
        harness.app.world_mut(),
        &w::DeviceOperationResult {
            operation_id: click.operation_id,
            output: json!({"applied":true}).to_string(),
            ..result
        },
    )
    .unwrap();
    assert_eq!(late.status, w::DeviceOperationStatus::Uncertain);
}

fn setup_computer_use_model(harness: &mut Harness, frame_id: &str) -> MockCompletionModel {
    use serde_json::json;
    let model = computer_model(
        harness,
        [
            tool_turn(
                "shot",
                "device_request",
                json!({
                    "capability": w::COMPUTER_USE, "input": {"action":"screenshot"}
                }),
            ),
            tool_turn(
                "click",
                "device_request",
                json!({
                    "capability": w::COMPUTER_USE,
                    "input": {"action":"click","frame_id":frame_id,"x":0,"y":0,"button":"left"}
                }),
            ),
        ],
        true,
    );
    register(
        harness.app.world_mut(),
        "desk",
        w::ClientCapabilitySpec {
            approval: w::CapabilityApproval::Auto.into(),
            ..spec(w::COMPUTER_USE)
        },
    );
    activate(harness.app.world_mut(), &harness.companion, "desk");
    model
}

fn wait_for_desk_operation(harness: &mut Harness) {
    harness.run_until(|world| {
        world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("desk")
            .unwrap()
            .len()
            == 1
    });
}

fn complete_screenshot_op(
    harness: &mut Harness,
    operation_id: &str,
    frame_id: &str,
) -> (w::DeviceOperationResult, String) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode(&[200, 0, 80], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    let encoded = STANDARD.encode(jpeg);
    let result = w::DeviceOperationResult {
        client_id: "desk".into(),
        operation_id: operation_id.to_owned(),
        success: true,
        output: json!({"frame_id":frame_id,"width":1,"height":1,"image_base64":encoded})
            .to_string(),
        ..Default::default()
    };
    let state = device::complete(harness.app.world_mut(), &result.clone()).unwrap();
    assert_eq!(state.status, w::DeviceOperationStatus::Completed);
    assert_eq!(
        state.result.unwrap().output,
        json!({"frame_id":frame_id,"width":1,"height":1}).to_string()
    );
    let saved = harness
        .store()
        .client_repo()
        .operation(&result.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.output.unwrap(),
        json!({"frame_id":frame_id,"width":1,"height":1})
    );
    (result, encoded)
}

struct ScreenshotAudit<'a> {
    operation_id: &'a str,
    run: &'a str,
    frame_id: &'a str,
    encoded: &'a str,
}

fn assert_redacted_screenshot_effects(
    harness: &Harness,
    model: &MockCompletionModel,
    events: &mut tokio::sync::broadcast::Receiver<w::Notification>,
    audit: &ScreenshotAudit<'_>,
) {
    let &ScreenshotAudit {
        operation_id,
        run,
        frame_id,
        encoded,
    } = audit;
    use rig_core::message::UserContent;
    assert!(model.requests().iter().any(|request| {
        request.chat_history.iter().any(|message| {
            matches!(message.as_ref(), Message::User { content } if content.iter().any(|part| matches!(part, UserContent::Image(_))))
        })
    }));
    let mut result_event = false;
    while let Ok(notification) = events.try_recv() {
        if let Some(w::notification::Event::DeviceOperationResult(result)) = notification.event
            && result.operation_id == operation_id
        {
            assert!(!result.output.contains(encoded));
            result_event = true;
        }
    }
    assert!(
        result_event,
        "observers received only the screenshot metadata"
    );
    let logs = harness.root.path().join("rig").join("logs");
    let work_log =
        std::fs::read_to_string(logs.join(format!("{}.jsonl", agent::work_agent_id(run)))).unwrap();
    assert!(work_log.contains(frame_id));
    assert!(!work_log.contains(encoded));
}

#[test]
fn computer_use_respects_the_client_declaration_model_and_host_denial() {
    use serde_json::json;

    for (declared, image, denied) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let mut harness = Harness::new([]);
        let model = computer_model(
            &mut harness,
            [
                tool_turn(
                    "shot",
                    "device_request",
                    json!({
                        "capability": w::COMPUTER_USE, "input":{"action":"screenshot"}
                    }),
                ),
                text_turn("cannot act"),
            ],
            image,
        );
        let spec = if declared {
            w::ClientCapabilitySpec {
                approval: w::CapabilityApproval::Auto.into(),
                ..spec(w::COMPUTER_USE)
            }
        } else {
            spec("body.motion")
        };
        register(harness.app.world_mut(), "desk", spec);
        activate(harness.app.world_mut(), &harness.companion, "desk");
        let hub = harness.app.world().resource::<DeviceHub>().clone();
        let (_, mut stream) = hub.register(&ene_domain::ClientId::parse("desk").unwrap());
        if denied {
            harness
                .store()
                .policy_repo()
                .apply_policy_rules(
                    &[permission::PolicyRule {
                        id: "deny-computer".into(),
                        tool_name: w::COMPUTER_USE.into(),
                        pattern: "*".into(),
                        effect: permission::PolicyEffect::Deny,
                        created_at: String::new(),
                    }],
                    &[],
                )
                .unwrap();
        }
        let session = start_work(&mut harness);
        harness.run_until(|world| {
            world
                .resource::<Store>()
                .work_repo()
                .active_run(&session.id)
                .unwrap()
                .is_none()
        });
        assert_ne!(
            model.requests(),
            [] as [rig_core::completion::CompletionRequest; 0]
        );
        let tools = &model.requests()[0].tools;
        let presented = tools.iter().any(|tool| {
            tool.name.as_str() == "device_request"
                && tool.parameters.to_string().contains(w::COMPUTER_USE)
        });
        assert_eq!(presented, declared && image);
        assert!(stream.try_recv().is_err());
        assert_eq!(
            harness
                .store()
                .client_repo()
                .outstanding_operations("desk")
                .unwrap(),
            [] as [ene_domain::DeviceOperation; 0]
        );
    }
}

#[test]
fn a_work_run_reports_back_to_the_parent() {
    let mut harness = Harness::new([text_turn("終わったよ")]);
    harness.app.update();
    let session = start_work(&mut harness);
    harness.run_until(|world| {
        world
            .resource::<Store>()
            .work_repo()
            .runs(&session.id)
            .unwrap()
            .iter()
            .any(|run| run.state.is_terminal())
    });
    harness.app.update();
    let parent = agent::agent_of(harness.app.world_mut(), &harness.companion).unwrap();
    let reported = harness
        .app
        .world()
        .get::<Conversation>(parent)
        .unwrap()
        .entries()
        .any(|entry| {
            entry
                .author
                .is_some_and(|author| author.kind == rig_core::conversation::AuthorKind::Agent)
        });
    assert!(reported, "the hand reports to the head");
}

#[test]
fn a_conflicting_client_tool_declaration_rolls_back_activation_and_replacement() {
    let mut harness = Harness::with_policy([], "*");
    let second = crate::host::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "kai".into(),
            mind: "calm".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    let declaration = |id: &str, description: &str| w::RegisterClientRequest {
        id: id.into(),
        name: id.into(),
        platform: "linux".into(),
        capabilities: vec!["body.motion".into()],
        capability_specs: vec![w::ClientCapabilitySpec {
            name: "body.motion".into(),
            tool_name: "body_motion".into(),
            description: description.into(),
            input_schema: "{}".into(),
            approval: w::CapabilityApproval::Auto.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    client::declare(
        harness.app.world_mut(),
        client::Declaration::Register(&declaration("c1", "ene moves")),
    )
    .unwrap();
    let second_client = client::declare(
        harness.app.world_mut(),
        client::Declaration::Register(&declaration("c2", "kai moves")),
    )
    .unwrap();
    crate::host::client::activate(
        harness.app.world_mut(),
        &w::ActivationRequest {
            companion_id: harness.companion.clone(),
            client_id: "c1".into(),
            ..Default::default()
        },
    )
    .unwrap();
    // The second activation would publish the same tool name with a different
    // declaration: the move is refused and its row must be gone.
    assert!(
        crate::host::client::activate(
            harness.app.world_mut(),
            &w::ActivationRequest {
                companion_id: second.clone(),
                client_id: "c2".into(),
                ..Default::default()
            },
        )
        .is_err()
    );
    let store = harness.store();
    assert!(store.client_repo().activation(&second).unwrap().is_none());
    assert_eq!(
        store
            .client_repo()
            .activation(&harness.companion)
            .unwrap()
            .unwrap()
            .client_id
            .as_deref(),
        Some("c1")
    );

    let replace = |revision, description| {
        let request = declaration("c2", description);
        w::SetCapabilitiesRequest {
            client_id: request.id,
            capabilities: request.capabilities,
            capability_specs: request.capability_specs,
            revision,
            ..Default::default()
        }
    };
    let shared = client::declare(
        harness.app.world_mut(),
        client::Declaration::Replace(&replace(second_client.revision, "ene moves")),
    )
    .unwrap();
    activate(harness.app.world_mut(), &second, "c2");
    let conflict = client::declare(
        harness.app.world_mut(),
        client::Declaration::Replace(&replace(shared.revision, "kai moves")),
    );
    assert!(conflict.is_err());
    let persisted = store.client_repo().client("c2").unwrap().unwrap();
    assert_eq!(persisted.revision, shared.revision);
    assert_eq!(persisted.capability_specs[0].description, "ene moves");
}

fn unassigned(id: &str) -> crate::host::mind::Companion {
    crate::host::mind::Companion {
        id: id.into(),
        name: "Mio".into(),
        mind: "kind".into(),
        profile: crate::host::mind::Profile::default(),
        profile_revision: 1,
        state: crate::host::mind::State::default(),
        state_revision: 1,
        body: ene_mind::JsonDoc::empty(),
        voice: ene_mind::JsonDoc::empty(),
        assignments: crate::host::mind::ModelAssignments::default(),
        persona: None,
        voice_identifier: None,
        revision: 1,
        created_at: ene_store::util::now(),
        updated_at: ene_store::util::now(),
    }
}

fn notice_code(event: &w::Notification) -> Option<String> {
    let w::notification::Event::Generic(host) = event.event.as_ref()? else {
        return None;
    };
    if host.kind != "notice" {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_str(&host.payload.as_option()?.json).ok()?;
    payload.get("code")?.as_str().map(ToOwned::to_owned)
}

#[test]
fn a_companion_survives_a_create_and_an_edit_without_a_bound_model() {
    let mut harness = Harness::unbound();
    let before = harness
        .app
        .world()
        .resource::<crate::host::notify::Notifier>()
        .cursor();
    let renamed = crate::host::mind::update(
        harness.app.world_mut(),
        w::UpdateCompanionRequest {
            id: harness.companion.clone(),
            revision: 1,
            name: Some("Mio II".into()),
            ..Default::default()
        },
    )
    .expect("an edit to a companion whose model is not bound yet is still saved");
    assert_eq!(renamed.name, "Mio II");
    let companion = crate::host::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "Mio".into(),
            mind: "kind".into(),
            ..Default::default()
        },
    )
    .expect("a companion whose model is not bound yet is still created")
    .id;
    crate::host::tick(&mut harness.app);
    // The row is the source of truth; the model component is not.
    let store = harness.store();
    assert!(
        store
            .companion_repo()
            .companion(&companion)
            .unwrap()
            .is_some()
    );
    let entity = agent::agent_of(harness.app.world_mut(), &companion).expect("an agent");
    assert!(
        harness
            .app
            .world()
            .get::<rig_ecs::model::Connection>(entity)
            .is_none()
    );
    // The client learns why the companion cannot reply yet.
    let replay = harness
        .app
        .world()
        .resource::<crate::host::notify::Notifier>()
        .replay(&before)
        .expect("the notice is in the replay ring");
    assert!(
        replay
            .iter()
            .any(|event| notice_code(event).as_deref() == Some("model_unavailable")),
        "the client is told the model is unavailable"
    );
}

#[tokio::test]
async fn a_stored_companion_without_a_model_does_not_block_startup() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path();
    {
        let store = Store::open(&state.join("host.sqlite3")).unwrap();
        store
            .companion_repo()
            .insert_companion(&unassigned("mio"))
            .unwrap();
    }
    let runtime =
        crate::host::Runtime::start_with(state, crate::host::resources::Options::default())
            .await
            .expect("a companion without a model must not stop the host from starting");
    runtime.shutdown().await.unwrap();
}

#[test]
fn a_delivery_without_a_bound_model_starts_no_turn() {
    let mut unbound = Harness::unbound();
    let agent = agent::agent_of(unbound.app.world_mut(), &unbound.companion).expect("an agent");
    let before = unbound
        .app
        .world()
        .resource::<crate::host::notify::Notifier>()
        .cursor();
    // The delivery voice and the schedule loop make: no `chat` call first.
    let delivered = agent::deliver(
        unbound.app.world_mut(),
        agent,
        &unbound.companion,
        "reminder".into(),
        DeliveryMode::Queue,
        Author::plugin("ene-host"),
    );
    crate::host::tick(&mut unbound.app);
    assert!(!delivered, "a turn does not start without a bound model");
    let history = history_texts(&mut unbound);
    assert!(
        !history.iter().any(|text| text.contains("reminder")),
        "the turn never reaches the conversation: {history:?}"
    );
    let replay = unbound
        .app
        .world()
        .resource::<crate::host::notify::Notifier>()
        .replay(&before)
        .expect("the notice is in the replay ring");
    assert!(
        replay
            .iter()
            .any(|event| notice_code(event).as_deref() == Some("model_unavailable")),
        "the client is told why the turn did not start"
    );

    // The same delivery to a bound agent runs a turn, so this test can see one
    // when it happens.
    let mut bound = Harness::new([text_turn("ok")]);
    let agent = agent::agent_of(bound.app.world_mut(), &bound.companion).expect("an agent");
    let delivered = agent::deliver(
        bound.app.world_mut(),
        agent,
        &bound.companion,
        "reminder".into(),
        DeliveryMode::Queue,
        Author::plugin("ene-host"),
    );
    assert!(delivered, "a bound agent takes the turn");
    let mut history = Vec::new();
    for _ in 0..40 {
        crate::host::tick(&mut bound.app);
        history = history_texts(&mut bound);
        if history.iter().any(|text| text == "ok") {
            break;
        }
    }
    assert!(
        history.iter().any(|text| text == "ok"),
        "the delivered message runs a turn: {history:?}"
    );
}
