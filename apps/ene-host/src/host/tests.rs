//! Behavior tests on a real headless app: the ECS loop, the gate, the
//! conversation log and the SQLite state together.
#![cfg(test)]

use super::{
    agent::{self, TestModel},
    chat, client,
    client::device,
    db::Store,
    permission,
    resources::{DeviceHub, Perception, Waiting},
    work,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::catalog::Catalog;
use rig_core::completion::Message;
use rig_core::operation::Completion;
use rig_core::serve::ErasedHandler;
use rig_core::serve::adapters::ModelAdapter;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_ecs::effects::Handler;
use rig_ecs::prelude::*;
use rig_ecs::session_log::SessionStore;
use rig_memory::DirStore;
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

pub(crate) struct Harness {
    pub(crate) app: App,
    companion: String,
    wakes: mpsc::Receiver<()>,
    _root: tempfile::TempDir,
}

impl Harness {
    pub(crate) fn new(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) -> Self {
        Self::with_policy(turns, "*")
    }

    /// The default harness grants every declaration; a test that checks the
    /// policy passes the exact `ENE_HOST_ALLOWED_CAPABILITIES` value instead.
    fn with_policy(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>, policy: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let model = MockCompletionModel::from_stream_turns(turns);
        let state = root.path();
        let store = Store::open(&state.join("host.sqlite3")).unwrap();
        let logs = DirStore::open(state.join("rig")).unwrap();
        let (sender, wakes) = mpsc::channel();
        let mut app = App::new();
        app.set_error_handler(bevy_ecs::error::warn)
            .add_plugins(TaskPoolPlugin::default())
            .insert_resource(store)
            .insert_resource(super::model::Credentials::default())
            .insert_resource(client::CapabilityPolicy::parse(policy))
            .insert_resource(DeviceHub::default())
            .insert_resource(super::HostState::default())
            .insert_resource(super::notify::Notifier::new())
            .insert_resource(Waiting::default())
            .insert_resource(Perception::default())
            .insert_resource(permission::Approvals::default())
            .insert_resource(super::resources::DeviceTimeout(Duration::from_millis(100)))
            .insert_resource(rig_ecs::tasks::Wake::new(move || {
                let _ = sender.send(());
            }))
            .insert_resource(SessionStore::new(logs))
            .insert_resource(TestModel(Arc::new(move || bound(&model))))
            .add_plugins((
                AgentPlugin,
                SessionLogPlugin,
                agent::AgentPlugin,
                permission::GatePlugin,
                client::ClientPlugin,
                work::WorkPlugin,
                super::mind::MindPlugin,
                super::compaction::CompactionPlugin,
            ));
        app.finish();
        if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
            time.set_max_delta(Duration::from_secs(60));
        }
        app.update();
        let companion = super::mind::create(
            app.world_mut(),
            w::CreateCompanionRequest {
                name: "ene".into(),
                mind: "kind".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .id;
        app.update();
        Self {
            app,
            companion,
            wakes,
            _root: root,
        }
    }

    /// An app that binds models for real: no `TestModel`, so `ensure_model`
    /// reads the stored assignments and the host's own environment, as the
    /// running host does.
    fn unbound() -> Self {
        let mut harness = Self::with_policy([], "*");
        harness.app.world_mut().remove_resource::<TestModel>();
        harness
    }

    pub(crate) fn accept(&mut self, key: &str, text: &str) -> super::error::Result<String> {
        Ok(chat::accept(
            &mut self.app,
            w::SendMessageRequest {
                companion_id: self.companion.clone(),
                request_key: key.into(),
                text: text.into(),
                ..Default::default()
            },
        )?
        .request_id)
    }

    pub(crate) fn run_until(&mut self, done: impl Fn(&mut World) -> bool) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            self.app.update();
            if done(self.app.world_mut()) {
                return;
            }
            let _ = self.wakes.recv_timeout(Duration::from_millis(20));
        }
        let failed = self
            .app
            .world()
            .resource::<super::HostState>()
            .failed
            .clone();
        panic!("the condition did not hold in time; failed={failed:?}");
    }

    fn store(&self) -> Store {
        self.app.world().resource::<Store>().clone()
    }

    /// Advances the loop until a message with exactly `text` is in the log.
    /// `answered` only says that *some* reply landed, so a turn that answers
    /// twice needs this instead of a second bare `update`.
    fn run_until_text(&mut self, text: &str) {
        let companion = self.companion.clone();
        self.run_until(move |world| {
            chat::history(
                world,
                w::HistoryRequest {
                    companion_id: companion.clone(),
                    ..Default::default()
                },
            )
            .is_ok_and(|history| history.items.iter().any(|item| item.text == text))
        });
    }

    fn history(&mut self) -> w::History {
        chat::history(
            self.app.world_mut(),
            w::HistoryRequest {
                companion_id: self.companion.clone(),
                ..Default::default()
            },
        )
        .unwrap()
    }
}

fn bound(model: &MockCompletionModel) -> super::model::Bound {
    let spec = Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .shared();
    let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
        spec.reference(),
        model.clone(),
    ));
    super::model::Bound {
        connection: rig_ecs::model::Connection {
            spec: Arc::clone(&spec),
            handler: Handler(handler),
        },
    }
}

fn answered(world: &mut World) -> bool {
    let mut query = world.query::<&Conversation>();
    query.iter(world).any(|conversation| {
        conversation
            .entries()
            .any(|entry| matches!(entry.message, Message::Assistant(_)))
    })
}

fn text_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// Turns for a work run that issues a gated `device_request` and then reports.
fn work_device_turns() -> [Vec<MockStreamEvent>; 2] {
    [
        vec![
            MockStreamEvent::tool_call(
                "call-1",
                "device_request",
                serde_json::json!({"capability": "device.files", "input": {"path": "/tmp/a"}}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        text_turn("終わったよ"),
    ]
}

fn spec(name: &str) -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        name: name.into(),
        description: format!("{name} capability"),
        approval_target: "{path}".into(),
        ..Default::default()
    }
}

/// A declared capability that always needs a Stage::Gate approval.
fn files_capability() -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        approval: w::CapabilityApproval::Confirm.into(),
        input_schema:
            r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}"#
                .into(),
        ..spec("device.files")
    }
}

/// A granted capability published as a tool named `tool`, run without a card.
fn capability_tool(capability: &str, tool: &str) -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        tool_name: tool.into(),
        approval: w::CapabilityApproval::Auto.into(),
        ..spec(capability)
    }
}

/// The client and generation a single-companion capability tool serves.
fn published_tool(world: &mut World, tool: &str) -> Option<(String, u64)> {
    let mut query = world.query::<(&rig_ecs::tools::ToolDef, &client::dynamic::CapabilityTool)>();
    query
        .iter(world)
        .find(|(definition, _)| definition.0.name.as_str() == tool)
        .and_then(|(_, published)| published.bindings.first())
        .map(|binding| (binding.client_id.clone(), binding.activation_revision))
}

/// Every active companion/client generation a published tool dispatches for.
fn published_bindings(world: &mut World, tool: &str) -> Vec<client::dynamic::CapabilityBinding> {
    let mut query = world.query::<(&rig_ecs::tools::ToolDef, &client::dynamic::CapabilityTool)>();
    query
        .iter(world)
        .find(|(definition, _)| definition.0.name.as_str() == tool)
        .map(|(_, published)| published.bindings.clone())
        .unwrap_or_default()
}

/// A model turn that calls `tool` and ends the model call.
fn tool_turn(id: &str, tool: &str, input: serde_json::Value) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::tool_call(id, tool, input),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// Whether any conversation holds an assistant message with exactly `text`.
fn said(world: &mut World, text: &str) -> bool {
    let mut query = world.query::<&Conversation>();
    query.iter(world).any(|conversation| {
        conversation
            .entries()
            .any(|entry| agent::entry_text(&entry).as_deref() == Some(text))
    })
}

/// The tools a companion's parent agent may call.
fn access_of(world: &mut World, companion: &str) -> ToolAccess {
    let entity = agent::agent_of(world, companion).expect("the companion has an agent");
    world
        .get::<ToolAccess>(entity)
        .cloned()
        .expect("a parent has a tool access")
}

/// Waits for the next pending approval and returns it.
fn pending_approval(harness: &mut Harness) -> permission::Approval {
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty()
    });
    harness
        .store()
        .approvals(permission::ApprovalStatus::Pending)
        .unwrap()[0]
        .clone()
}

fn register(world: &mut World, id: &str, spec: w::ClientCapabilitySpec) -> w::Client {
    let capability = spec.name.clone();
    client::register(
        world,
        w::RegisterClientRequest {
            id: id.into(),
            name: id.into(),
            platform: "linux".into(),
            capabilities: vec![capability],
            capability_specs: vec![spec],
            ..Default::default()
        },
    )
    .unwrap()
}

fn activate(
    world: &mut World,
    companion: &str,
    client: &str,
    expected_revision: u64,
) -> w::Activation {
    client::activate(
        world,
        w::ActivationRequest {
            companion_id: companion.into(),
            client_id: client.into(),
            expected_revision,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn a_turn_is_accepted_once_and_answered() {
    let mut harness = Harness::new([text_turn("こんにちは")]);
    let first = harness.accept("key-1", "hi").unwrap();
    assert!(!first.is_empty());
    assert_eq!(harness.accept("key-1", "hi").unwrap(), first);
    assert!(harness.accept("key-1", "different").is_err());
    harness.run_until(answered);
    harness.app.update();
    let history = harness.history();
    assert!(history.items.iter().any(|item| item.text == "こんにちは"));
    assert!(
        history
            .items
            .iter()
            .any(|item| item.text == "hi" && item.role == w::MessageRole::User)
    );
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
    activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let (subscription, mut stream) = harness
        .app
        .world_mut()
        .resource::<DeviceHub>()
        .register("c1");
    // The turn parks on the approval before the operation is dispatched.
    harness.accept("key-1", "ファイルに書いて").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
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
        .approvals(permission::ApprovalStatus::Pending)
        .unwrap()[0]
        .clone();
    assert_eq!(request.capability.as_deref(), Some("device.files"));
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
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let event = stream.try_recv().expect("an operation reaches the client");
    let Some(w::device_event::Event::Operation(operation)) = event.event else {
        panic!("expected an operation");
    };
    assert_eq!(operation.subscription_id, subscription);
    assert_eq!(operation.capability, "device.files");
    // The client's result answers the waiting tool call.
    let complete = device::complete(
        harness.app.world_mut(),
        w::DeviceOperationResult {
            client_id: "c1".into(),
            operation_id: operation.operation_id.clone(),
            success: true,
            output: r#"{"ok":true}"#.into(),
            activation_revision: operation.activation_revision,
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
    let client = client::register(
        harness.app.world_mut(),
        w::RegisterClientRequest {
            id: "c1".into(),
            name: "desk".into(),
            platform: "linux".into(),
            capabilities: vec!["body.motion".into(), "device.files".into()],
            capability_specs: vec![
                capability_tool("body.motion", "body_motion"),
                capability_tool("device.files", "device_files"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(client.capabilities, ["body.motion", "device.files"]);
    assert_eq!(client.granted_capabilities, ["body.motion"]);
    // A declaration alone publishes nothing; only a granted capability of the
    // active client becomes a tool.
    assert_eq!(published_tool(harness.app.world_mut(), "body_motion"), None);
    let activation = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some(("c1".into(), activation.revision))
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
    let first = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let session = start_work(&mut harness);
    // The hand stays bound to c1/rev1; only the head moves to c2.
    let second = activate(
        harness.app.world_mut(),
        &harness.companion,
        "c2",
        first.revision,
    );
    assert_eq!(second.revision, first.revision + 1);
    let request = pending_approval(&mut harness);
    let run = harness.store().runs(&session.id).unwrap()[0].run_id.clone();
    assert_eq!(request.run_id, run);
    assert_eq!(request.work_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(request.client_id.as_deref(), Some("c1"));
    assert_eq!(request.activation_revision, Some(first.revision));
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
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
}

#[test]
fn a_work_approval_without_a_stored_work_id_falls_back_to_its_run() {
    let mut harness = Harness::new(work_device_turns());
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let session = start_work(&mut harness);
    let request = pending_approval(&mut harness);
    assert_eq!(request.work_id.as_deref(), Some(session.id.as_str()));
    // A row from before the column existed still resolves through its run.
    harness
        .store()
        .conn()
        .unwrap()
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
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
}

#[test]
fn a_stored_work_id_is_validated_before_the_run_lookup() {
    let mut harness = Harness::new(work_device_turns());
    register(harness.app.world_mut(), "c1", files_capability());
    activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let _session = start_work(&mut harness);
    let request = pending_approval(&mut harness);
    // The stored work session is authoritative: a run that would still
    // resolve is not followed when the column names a session that does not.
    harness
        .store()
        .conn()
        .unwrap()
        .execute(
            "UPDATE approval_requests SET work_id = 'missing' WHERE request_id = ?1",
            [&request.request_id],
        )
        .unwrap();
    let error = permission::submit(
        harness.app.world_mut(),
        &request.request_id,
        permission::ApprovalDecision::Approve,
    )
    .unwrap_err();
    assert!(matches!(error, super::error::HostError::Conflict(_)));
    let expired = harness
        .store()
        .approval(&request.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(expired.status, permission::ApprovalStatus::Expired);
}

#[test]
fn moving_the_active_client_invalidates_the_old_generation() {
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
    let first = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    // The declared tool is a spawned tool published for the active generation.
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some(("c1".into(), first.revision))
    );
    assert!(
        !harness
            .app
            .world()
            .resource::<client::CapabilityTools>()
            .entities
            .keys()
            .any(|name| name.starts_with("ene_cap_")),
        "the fixed capability slots are gone"
    );
    let second = activate(
        harness.app.world_mut(),
        &harness.companion,
        "c2",
        first.revision,
    );
    assert_eq!(second.revision, first.revision + 1);
    // The tool moved with the active client instead of staying bound to c1.
    assert_eq!(
        published_tool(harness.app.world_mut(), "body_motion"),
        Some(("c2".into(), second.revision))
    );
    let activation = harness
        .store()
        .activation(&harness.companion)
        .unwrap()
        .unwrap();
    assert_eq!(activation.client_id.as_deref(), Some("c2"));
    // A perception event from the old generation is discarded.
    client::report_perception(
        harness.app.world_mut(),
        w::PerceptionEvent {
            client_id: "c1".into(),
            activation_revision: first.revision,
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
        w::ActivationRequest {
            companion_id: harness.companion.clone(),
            release: true,
            expected_revision: second.revision,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(published_tool(harness.app.world_mut(), "body_motion"), None);
    assert!(
        harness
            .app
            .world()
            .resource::<client::CapabilityTools>()
            .entities
            .is_empty()
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
    let activation = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    harness.accept("key-1", "ファイルに書いて").unwrap();
    let request = pending_approval(&mut harness);
    // The card names the spawned tool and its capability; a head call has no
    // work session.
    assert_eq!(request.tool_name, "device_files");
    assert_eq!(request.capability.as_deref(), Some("device.files"));
    assert_eq!(request.work_id, None);
    assert!(
        harness
            .store()
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
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let operation = harness.store().outstanding_operations("c1").unwrap()[0].clone();
    assert_eq!(operation.capability, "device.files");
    assert_eq!(operation.activation_revision, activation.revision);
    device::complete(
        harness.app.world_mut(),
        w::DeviceOperationResult {
            client_id: "c1".into(),
            operation_id: operation.id.clone(),
            success: true,
            output: r#"{}"#.into(),
            activation_revision: operation.activation_revision,
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
    let activation = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    harness.accept("key-1", "うなずいて").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    let operation = harness.store().outstanding_operations("c1").unwrap()[0].clone();
    assert_eq!(operation.capability, "body.motion");
    assert_eq!(operation.activation_revision, activation.revision);
    assert_eq!(operation.input, serde_json::json!({"motion": "nod"}));
    assert!(
        harness
            .store()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty(),
        "an Auto capability runs without a card"
    );
}

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
    let other = super::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "other".into(),
            mind: "kind".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    client::register(
        harness.app.world_mut(),
        w::RegisterClientRequest {
            id: "c1".into(),
            name: "c1".into(),
            platform: "linux".into(),
            capabilities: vec!["body.motion".into(), "eye.watch".into()],
            capability_specs: vec![
                capability_tool("body.motion", "body_motion"),
                capability_tool("eye.watch", "set_eye_watch"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    register(
        harness.app.world_mut(),
        "c2",
        capability_tool("body.motion", "body_motion"),
    );
    let first = activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let second = activate(harness.app.world_mut(), &other, "c2", 0);
    // One model-facing definition, one binding per companion generation.
    let bindings = published_bindings(harness.app.world_mut(), "body_motion");
    assert_eq!(bindings.len(), 2);
    for (companion, client, revision) in [
        (&harness.companion, "c1", first.revision),
        (&other, "c2", second.revision),
    ] {
        assert!(bindings.iter().any(|binding| {
            binding.companion_id == *companion
                && binding.client_id == client
                && binding.activation_revision == revision
        }));
    }
    // Both parents see the shared name; each sees only its own companion's
    // capability tools.
    let access_a = access_of(harness.app.world_mut(), &harness.companion);
    assert!(access_a.allows("body_motion"));
    assert!(access_a.allows("set_eye_watch"));
    let access_b = access_of(harness.app.world_mut(), &other);
    assert!(access_b.allows("body_motion"));
    assert!(
        !access_b.allows("set_eye_watch"),
        "another companion's capability tool must not be offered"
    );
    assert_eq!(
        published_bindings(harness.app.world_mut(), "set_eye_watch").len(),
        1
    );
    // A's call is dispatched to A's active client.
    harness.accept("key-a", "A please").unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .outstanding_operations("c1")
            .unwrap()
            .is_empty()
    });
    harness.run_until(|world| said(world, "A done"));
    assert!(
        harness
            .store()
            .outstanding_operations("c2")
            .unwrap()
            .is_empty(),
        "A's call must not reach B's client"
    );
    // B's call of the same tool name is dispatched to B's active client.
    chat::accept(
        &mut harness.app,
        w::SendMessageRequest {
            companion_id: other.clone(),
            request_key: "key-b".into(),
            text: "B please".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        !world
            .resource::<Store>()
            .outstanding_operations("c2")
            .unwrap()
            .is_empty()
    });
    harness.run_until(|world| said(world, "B done"));
    let a_operations = harness.store().outstanding_operations("c1").unwrap();
    assert_eq!(a_operations.len(), 1);
    assert_eq!(a_operations[0].capability, "body.motion");
    assert_eq!(a_operations[0].activation_revision, first.revision);
    assert_eq!(a_operations[0].input, serde_json::json!({"motion": "nod"}));
    let b_operations = harness.store().outstanding_operations("c2").unwrap();
    assert_eq!(b_operations.len(), 1);
    assert_eq!(b_operations[0].capability, "body.motion");
    assert_eq!(b_operations[0].activation_revision, second.revision);
    assert_eq!(b_operations[0].input, serde_json::json!({"motion": "wave"}));
    // A hand never sees a capability tool.
    let companion = harness
        .store()
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
    let other = super::mind::create(
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
    activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    let error = client::activate(
        harness.app.world_mut(),
        w::ActivationRequest {
            companion_id: other,
            client_id: "c2".into(),
            expected_revision: 0,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        super::error::HostError::Conflict(ref reason)
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
    activate(harness.app.world_mut(), &harness.companion, "c1", 0);
    // Keep the result-waiting deadline out of the assertion window.
    harness
        .app
        .world_mut()
        .resource_mut::<super::resources::DeviceTimeout>()
        .0 = Duration::from_secs(30);
    // A named capability tool answers immediately: the operation exists but
    // no tool call waits for its result.
    harness.accept("key-1", "move").unwrap();
    harness.run_until(|world| {
        world
            .resource::<Store>()
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
            .outstanding_operations("c1")
            .unwrap()
            .len()
            == 2
    });
    let operations = harness.store().outstanding_operations("c1").unwrap();
    let waiting = &harness.app.world().resource::<Waiting>().by_operation;
    assert_eq!(waiting.len(), 1);
    assert!(
        operations
            .iter()
            .any(|operation| waiting.contains_key(&operation.id)),
        "the waiting call belongs to the device_request operation"
    );
}

fn start_work(harness: &mut Harness) -> w::WorkSession {
    work::start(
        harness.app.world_mut(),
        w::StartWorkRequest {
            companion_id: harness.companion.clone(),
            title: "task".into(),
            instruction: "list the folder".into(),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn a_work_run_reports_back_to_the_parent() {
    let mut harness = Harness::new([text_turn("終わったよ")]);
    harness.app.update();
    let session = start_work(&mut harness);
    harness.run_until(|world| {
        world
            .resource::<Store>()
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
fn a_conflicting_activation_is_rolled_back_before_side_effects() {
    let mut harness = Harness::with_policy([], "*");
    let second = super::mind::create(
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
    super::client::register(harness.app.world_mut(), declaration("c1", "ene moves")).unwrap();
    super::client::register(harness.app.world_mut(), declaration("c2", "kai moves")).unwrap();
    super::client::activate(
        harness.app.world_mut(),
        w::ActivationRequest {
            companion_id: harness.companion.clone(),
            client_id: "c1".into(),
            ..Default::default()
        },
    )
    .unwrap();
    // The second activation would publish the same tool name with a different
    // declaration: the move is refused and its row must be gone.
    assert!(
        super::client::activate(
            harness.app.world_mut(),
            w::ActivationRequest {
                companion_id: second.clone(),
                client_id: "c2".into(),
                ..Default::default()
            },
        )
        .is_err()
    );
    let store = harness.store();
    assert!(store.activation(&second).unwrap().is_none());
    assert_eq!(
        store
            .activation(&harness.companion)
            .unwrap()
            .unwrap()
            .client_id
            .as_deref(),
        Some("c1")
    );
}

fn unassigned(id: &str) -> super::mind::Companion {
    super::mind::Companion {
        id: id.into(),
        name: "Mio".into(),
        mind: "kind".into(),
        profile: super::mind::Profile::default(),
        profile_revision: 1,
        state: super::mind::State::default(),
        state_revision: 1,
        body: super::json::JsonDoc::empty(),
        voice: super::json::JsonDoc::empty(),
        assignments: super::mind::Assignments::default(),
        persona: None,
        voice_identifier: None,
        revision: 1,
        created_at: super::db::now(),
        updated_at: super::db::now(),
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
        .resource::<super::notify::Notifier>()
        .cursor();
    let renamed = super::mind::update(
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
    let companion = super::mind::create(
        harness.app.world_mut(),
        w::CreateCompanionRequest {
            name: "Mio".into(),
            mind: "kind".into(),
            ..Default::default()
        },
    )
    .expect("a companion whose model is not bound yet is still created")
    .id;
    super::tick(&mut harness.app);
    // The row is the source of truth; the model component is not.
    let store = harness.store();
    assert!(store.companion(&companion).unwrap().is_some());
    let entity = agent::agent_of(harness.app.world_mut(), &companion).expect("an agent");
    assert!(harness.app.world().get::<agent::Binding>(entity).is_none());
    // The client learns why the companion cannot reply yet.
    let replay = harness
        .app
        .world()
        .resource::<super::notify::Notifier>()
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
        store.insert_companion(&unassigned("mio")).unwrap();
    }
    let runtime = super::Runtime::start_with(state, super::resources::Options::default())
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
        .resource::<super::notify::Notifier>()
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
    super::tick(&mut unbound.app);
    assert!(!delivered, "a turn does not start without a bound model");
    let history = history_texts(&mut unbound);
    assert!(
        !history.iter().any(|text| text.contains("reminder")),
        "the turn never reaches the conversation: {history:?}"
    );
    let replay = unbound
        .app
        .world()
        .resource::<super::notify::Notifier>()
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
        super::tick(&mut bound.app);
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

fn history_texts(harness: &mut Harness) -> Vec<String> {
    chat::history(
        harness.app.world_mut(),
        w::HistoryRequest {
            companion_id: harness.companion.clone(),
            ..Default::default()
        },
    )
    .expect("the history of a companion")
    .items
    .iter()
    .map(|item| item.text.clone())
    .collect()
}
