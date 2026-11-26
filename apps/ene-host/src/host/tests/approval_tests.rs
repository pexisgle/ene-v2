use super::device_tests::start_work;
use super::*;
use crate::host::{
    client,
    client::device,
    db::Store,
    permission,
    resources::{DeviceHub, Perception},
};
use ene_domain::ClientId;
use ene_rpc as w;
use rig_core::test_utils::MockStreamEvent;

#[tokio::test]
async fn a_real_guest_tool_call_is_held_and_denied_by_the_gate() {
    let plugin = crate::host::model::plugin_tests::Harness::start(None).await;
    let call = plugin.tool_call("test-confirm-tool").await;
    let mut harness = Harness::new(Vec::<Vec<MockStreamEvent>>::new());
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let (_subscription, mut stream) = harness
        .app
        .world_mut()
        .resource::<DeviceHub>()
        .register(&ClientId::parse("c1").unwrap());
    let agent = harness.companion_agent();
    let call_entity = harness
        .app
        .world_mut()
        .spawn((
            rig_ecs::tools::ToolRequest { call },
            rig_ecs::stage::Effect::default(),
            rig_ecs::stage::ForAgent(agent),
            rig_ecs::stage::Admitted,
        ))
        .id();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    assert!(
        stream.try_recv().is_err(),
        "nothing may be dispatched before approval"
    );
    let request = pending_approval(&mut harness);
    let approval = permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Deny,
    )
    .unwrap();
    assert_eq!(approval.status, permission::ApprovalStatus::Denied);
    harness.app.update();
    assert!(
        stream.try_recv().is_err(),
        "a denied call must not be executed"
    );
    assert!(
        harness
            .app
            .world()
            .get::<rig_ecs::stage::Decision>(call_entity)
            .is_some()
    );
}

#[test]
fn the_gate_refuses_a_reply_over_the_call_count_limit() {
    use rig_core::completion::message::{CallId, ToolCall, ToolFunction, ToolName};
    use rig_ecs::stage::Decision;

    let mut harness = Harness::new(Vec::<Vec<MockStreamEvent>>::new());
    let agent = harness.companion_agent();
    let parent = harness
        .app
        .world_mut()
        .spawn(rig_ecs::stage::Effect::default())
        .id();
    let parent_id = harness
        .app
        .world()
        .get::<rig_ecs::stage::Effect>(parent)
        .unwrap()
        .id();
    let mut calls = Vec::new();
    for index in 0..65 {
        let call = ToolCall {
            id: CallId::from_wire(format!("call-{index}")),
            function: ToolFunction::new(
                ToolName::new("device_request").unwrap(),
                serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
            ),
            native: None,
        };
        let entity = harness
            .app
            .world_mut()
            .spawn((
                rig_ecs::tools::ToolRequest { call },
                rig_ecs::stage::Effect::with_parent_id(Some(parent_id)),
                rig_ecs::stage::ForAgent(agent),
                rig_ecs::stage::Admitted,
            ))
            .id();
        calls.push(entity);
    }
    harness.app.update();
    for call in calls {
        let decision = harness
            .app
            .world()
            .get::<Decision>(call)
            .expect("the gate decided");
        let report = decision.get().expect("a decision").0.as_ref().unwrap_err();
        assert!(report.to_string().contains("tool-call count"), "{report}");
    }
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap(),
        [] as [ene_policy::Approval; 0]
    );
}

#[test]
fn the_gate_refuses_an_identifier_over_the_length_limit() {
    use rig_core::completion::message::{CallId, ToolCall, ToolFunction, ToolName};
    use rig_ecs::stage::Decision;

    let mut harness = Harness::new(Vec::<Vec<MockStreamEvent>>::new());
    let agent = harness.companion_agent();
    let call = ToolCall {
        id: CallId::from_wire("x".repeat(257)),
        function: ToolFunction::new(
            ToolName::new("device_request").unwrap(),
            serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
        ),
        native: None,
    };
    let call = harness
        .app
        .world_mut()
        .spawn((
            rig_ecs::tools::ToolRequest { call },
            rig_ecs::stage::Effect::default(),
            rig_ecs::stage::ForAgent(agent),
            rig_ecs::stage::Admitted,
        ))
        .id();
    harness.app.update();
    let decision = harness
        .app
        .world()
        .get::<Decision>(call)
        .expect("the gate decided");
    let report = decision.get().expect("a decision").0.as_ref().unwrap_err();
    assert!(report.to_string().contains("identifier length"), "{report}");
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap(),
        [] as [ene_policy::Approval; 0]
    );
}

#[test]
fn a_capability_tool_name_over_64_is_rejected_at_registration() {
    let mut harness = Harness::new(Vec::<Vec<MockStreamEvent>>::new());
    let result = client::declare(
        harness.app.world_mut(),
        client::Declaration::Register(&w::RegisterClientRequest {
            id: "c1".into(),
            name: "c1".into(),
            platform: "linux".into(),
            capabilities: vec!["device.files".into()],
            capability_specs: vec![capability_tool("device.files", &"n".repeat(65))],
            ..Default::default()
        }),
    );
    assert!(result.is_err());
}

#[test]
fn a_confirm_capability_is_gated_and_delivered_to_the_bound_client() {
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "device_request",
                serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("書いたよ"),
    ]);
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let (subscription, mut stream) = harness
        .app
        .world_mut()
        .resource::<DeviceHub>()
        .register(&ClientId::parse("c1").unwrap());
    // The turn parks on the approval before the operation is dispatched.
    harness.accept("key-1", "ファイルに書いて").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    assert!(
        stream.try_recv().is_err(),
        "nothing may be dispatched before approval"
    );
    let request = harness
        .store()
        .approval_repo()
        .approvals(permission::ApprovalStatus::Pending)
        .unwrap()[0]
        .clone();
    assert_eq!(request.capability.as_deref(), Some("device.files"));
    assert_eq!(
        request.normalized_arguments,
        permission::canonicalise(
            "device_request",
            &serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
        )
        .0,
        "the stored arguments are the same canonical form the hash covers"
    );
    let approval = permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    assert_eq!(approval.status, permission::ApprovalStatus::Approved);
    // The approved call runs and is delivered to the live stream.
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let event = stream.try_recv().expect("an operation reaches the client");
    let Some(w::device_event::Event::Operation(operation)) = event.event else {
        panic!("expected an operation");
    };
    assert!(subscription.matches_wire(&operation.subscription_id));
    assert_eq!(operation.capability, "device.files");
    // The client's result answers the waiting tool call.
    let complete = device::complete(
        harness.app.world_mut(),
        &w::DeviceOperationResult {
            client_id: "c1".into(),
            operation_id: operation.operation_id.clone(),
            success: true,
            output: r#"{"ok":true}"#.into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(complete.status, w::DeviceOperationStatus::Completed);
    harness.run_until_text("書いたよ");
}

#[test]
fn a_client_is_granted_only_what_the_host_policy_allows() {
    let mut harness = Harness::with_policy([], "body.motion");
    let client = client::declare(
        harness.app.world_mut(),
        client::Declaration::Register(&w::RegisterClientRequest {
            id: "c1".into(),
            name: "desk".into(),
            platform: "linux".into(),
            capabilities: vec!["body.motion".into(), "device.files".into()],
            capability_specs: vec![
                capability_tool("body.motion", "body_motion"),
                capability_tool("device.files", "device_files"),
            ],
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(client.capabilities, ["body.motion", "device.files"]);
    assert_eq!(client.granted_capabilities, ["body.motion"]);
    // A declaration alone publishes nothing; only a granted capability of the
    // active client becomes a tool.
    assert_eq!(published_tool(harness.app.world_mut(), "body_motion"), None);
    activate(harness.app.world_mut(), &harness.companion, "c1");
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some("c1".into())
    );
    assert_eq!(
        published_tool(harness.app.world_mut(), "device_files"),
        None
    );
}

#[test]
fn a_work_approval_is_bound_to_the_work_session_not_the_head_activation() {
    let mut harness = Harness::new(work_device_turns());
    for id in ["c1", "c2"] {
        register(harness.app.world_mut(), id, files_capability());
    }
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let session = start_work(&mut harness);
    // The hand stays bound to c1; only the head moves to c2.
    activate(harness.app.world_mut(), &harness.companion, "c2");
    let request = pending_approval(&mut harness);
    let run = harness.store().work_repo().runs(&session.id).unwrap()[0]
        .run_id
        .clone();
    assert_eq!(request.run_id, run);
    assert_eq!(request.work_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(request.client_id.as_deref(), Some("c1"));
    let approval = permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    assert_eq!(approval.status, permission::ApprovalStatus::Approved);
    // The approved operation still targets the hand's client.
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
}

#[test]
fn a_work_approval_without_a_stored_work_id_falls_back_to_its_run() {
    let mut harness = Harness::new(work_device_turns());
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let session = start_work(&mut harness);
    let request = pending_approval(&mut harness);
    assert_eq!(request.work_id.as_deref(), Some(session.id.as_str()));
    // A row from before the column existed still resolves through its run.
    harness
        .store()
        .conn()
        .execute(
            "UPDATE approval_requests SET work_id = NULL WHERE request_id = ?1",
            [&request.request_id],
        )
        .unwrap();
    permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
}

#[test]
fn an_approval_whose_stored_session_is_gone_still_runs_on_its_client() {
    let mut harness = Harness::new(work_device_turns());
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1");
    start_work(&mut harness);
    let request = pending_approval(&mut harness);
    // The stored work session names a session that does not exist. The
    // approval is still the user's: it runs on the client the call was
    // accepted for instead of being refused or sent elsewhere.
    harness
        .store()
        .conn()
        .execute(
            "UPDATE approval_requests SET work_id = 'missing' WHERE request_id = ?1",
            [&request.request_id],
        )
        .unwrap();
    permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approval(&request.request_id)
            .unwrap()
            .unwrap()
            .status,
        permission::ApprovalStatus::Approved
    );
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
}

#[test]
fn the_head_resolves_its_own_pending_approval_with_the_tool() {
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "device_request",
                serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("書いたよ"),
    ]);
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1");
    let agent = harness.companion_agent();
    // The tool is registered and offered to the head; it is a host tool, not a
    // client capability, so a declaration cannot shadow it.
    let registered = {
        let mut query = harness.app.world_mut().query::<&rig_ecs::tools::ToolDef>();
        query
            .iter(harness.app.world())
            .any(|definition| definition.0.name.as_str() == "resolve_approval")
    };
    assert!(registered, "resolve_approval is registered");
    assert!(
        access_of(harness.app.world_mut(), &harness.companion).allows("resolve_approval"),
        "the head is offered resolve_approval"
    );
    assert!(client::BUILTIN_TOOLS.contains(&"resolve_approval"));
    harness.accept("key-1", "ファイルに書いて").unwrap();
    let request = pending_approval(&mut harness);
    let answer = permission::resolve_approval(
        harness.app.world_mut(),
        agent,
        "call-resolve",
        &permission::ResolveApprovalArgs {
            request_id: request.request_id.clone(),
            decision: "approve".into(),
        },
    )
    .unwrap();
    assert!(answer.contains("approved"), "{answer}");
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approval(&request.request_id)
            .unwrap()
            .unwrap()
            .status,
        permission::ApprovalStatus::Approved
    );
    // The approved call runs and its operation reaches the bound client.
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    harness.run_until_text("書いたよ");
}

#[test]
fn the_head_cannot_resolve_another_companions_approval() {
    let mut harness = Harness::new([]);
    // A card of another companion, stored as the gate would store it.
    harness
        .store()
        .approval_repo()
        .insert_approval(&permission::Approval {
            request_id: "a1".into(),
            session_id: "other".into(),
            run_id: String::new(),
            work_id: None,
            action_payload_hash: vec![1, 2, 3],
            tool_name: "device_request".into(),
            normalized_arguments: r#"{"capability":"device.files"}"#.into(),
            status: permission::ApprovalStatus::Pending,
            created_at: ene_store::util::now(),
            resolved_at: None,
            client_id: Some("c2".into()),
            capability: Some("device.files".into()),
            external_destination: None,
        })
        .unwrap();
    let agent = harness.companion_agent();
    let resolve = |harness: &mut Harness, agent, decision: &str| {
        permission::resolve_approval(
            harness.app.world_mut(),
            agent,
            "call-resolve",
            &permission::ResolveApprovalArgs {
                request_id: "a1".into(),
                decision: decision.into(),
            },
        )
    };
    // Another companion's request is refused.
    assert!(matches!(
        resolve(&mut harness, agent, "approve"),
        Err(crate::host::error::HostError::Permission(_))
    ));
    // Anything but approve/deny is refused.
    assert!(matches!(
        resolve(&mut harness, agent, "maybe"),
        Err(crate::host::error::HostError::Invalid(_))
    ));
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approval("a1")
            .unwrap()
            .unwrap()
            .status,
        permission::ApprovalStatus::Pending,
        "a refused resolution leaves the card pending"
    );
}

#[test]
fn a_moved_client_keeps_a_held_approval_and_serves_the_new_active_client() {
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "device_request",
                serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("書いたよ"),
    ]);
    for id in ["c1", "c2"] {
        register(harness.app.world_mut(), id, files_capability());
    }
    activate(harness.app.world_mut(), &harness.companion, "c1");
    harness.accept("key-1", "ファイルに書いて").unwrap();
    let request = pending_approval(&mut harness);
    // The head moves while the card is pending: the hold survives the move.
    activate(harness.app.world_mut(), &harness.companion, "c2");
    assert_eq!(
        harness
            .store()
            .approval_repo()
            .approval(&request.request_id)
            .unwrap()
            .unwrap()
            .status,
        permission::ApprovalStatus::Pending,
        "a move does not expire the card"
    );
    permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    // The approved operation runs on the head's active client, and the client
    // the call was accepted for receives nothing.
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c2")
            .unwrap()
            .is_empty()
    });
    assert_eq!(
        harness
            .store()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap(),
        [] as [ene_domain::DeviceOperation; 0]
    );
    harness.run_until_text("書いたよ");
}
#[test]
fn moving_the_active_client_moves_the_published_tools() {
    let mut harness = Harness::new([]);
    for id in ["c1", "c2"] {
        register(
            harness.app.world_mut(),
            id,
            w::ClientCapabilitySpec {
                tool_name: "body_motion".into(),
                approval: w::CapabilityApproval::Auto.into(),
                ..spec("body.motion")
            },
        );
    }
    activate(harness.app.world_mut(), &harness.companion, "c1");
    // The declared tool is a spawned tool published for the active client.
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some("c1".into())
    );
    let world = harness.app.world_mut();
    assert!(
        world
            .query::<&client::dynamic::CapabilityTool>()
            .iter(world)
            .all(|tool| !tool.spec.tool_name.starts_with("ene_cap_")),
        "the fixed capability slots are gone"
    );
    activate(harness.app.world_mut(), &harness.companion, "c2");
    // The tool moved with the active client instead of staying bound to c1.
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some("c2".into())
    );
    let activation = harness
        .store()
        .client_repo()
        .activation(&harness.companion)
        .unwrap()
        .unwrap();
    assert_eq!(activation.client_id.as_deref(), Some("c2"));
    // A perception event from a client that is no longer active is discarded.
    client::report_perception(
        harness.app.world_mut(),
        &w::PerceptionEvent {
            client_id: "c1".into(),
            event: Some(w::perception_event::Event::ActiveWindow(Box::new(
                w::ActiveWindowChanged {
                    title: "old".into(),
                    app_id: "old".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        harness
            .app
            .world()
            .resource::<Perception>()
            .events
            .is_empty()
    );
    // Releasing the client despawns its capability tools.
    client::activate(
        harness.app.world_mut(),
        &w::ActivationRequest {
            companion_id: harness.companion.clone(),
            release: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(published_tool(harness.app.world_mut(), "body_motion"), None);
    let world = harness.app.world_mut();
    assert!(
        world
            .query::<&client::dynamic::CapabilityTool>()
            .iter(world)
            .next()
            .is_none()
    );
}

#[test]
fn a_capability_tool_call_is_dispatched_after_gate_approval() {
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "device_files",
                serde_json::json!({"path": "/tmp/a"}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("書いたよ"),
    ]);
    register(
        harness.app.world_mut(),
        "c1",
        w::ClientCapabilitySpec {
            tool_name: "device_files".into(),
            ..files_capability()
        },
    );
    activate(harness.app.world_mut(), &harness.companion, "c1");
    harness.accept("key-1", "ファイルに書いて").unwrap();
    let request = pending_approval(&mut harness);
    // The card names the spawned tool and its capability; a head call has no
    // work session.
    assert_eq!(request.tool_name, "device_files");
    assert_eq!(request.capability.as_deref(), Some("device.files"));
    assert_eq!(
        request.normalized_arguments,
        permission::canonicalise("device_files", &serde_json::json!({"path": "/tmp/a"})).0,
        "the stored arguments are the same canonical form the hash covers"
    );
    assert_eq!(request.work_id, None);
    assert!(
        harness
            .store()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty(),
        "nothing may reach the client before approval"
    );
    permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let operation = harness
        .store()
        .client_repo()
        .outstanding_operations("c1")
        .unwrap()[0]
        .clone();
    assert_eq!(operation.capability, "device.files");
    assert_eq!(operation.client_id, "c1");
    device::complete(
        harness.app.world_mut(),
        &w::DeviceOperationResult {
            client_id: "c1".into(),
            operation_id: operation.id.clone(),
            success: true,
            output: r#"{}"#.into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(answered);
}

#[test]
fn an_auto_capability_tool_becomes_a_device_operation() {
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "body_motion",
                serde_json::json!({"motion": "nod"}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("動いたよ"),
    ]);
    register(
        harness.app.world_mut(),
        "c1",
        capability_tool("body.motion", "body_motion"),
    );
    activate(harness.app.world_mut(), &harness.companion, "c1");
    harness.accept("key-1", "うなずいて").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let operation = harness
        .store()
        .client_repo()
        .outstanding_operations("c1")
        .unwrap()[0]
        .clone();
    assert_eq!(operation.capability, "body.motion");
    assert_eq!(operation.client_id, "c1");
    assert_eq!(operation.input, serde_json::json!({"motion": "nod"}));
    assert!(
        harness
            .store()
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty(),
        "an Auto capability runs without a card"
    );
}
