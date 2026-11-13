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

struct Harness {
    app: App,
    companion: String,
    wakes: mpsc::Receiver<()>,
    _root: tempfile::TempDir,
}

impl Harness {
    fn new(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) -> Self {
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

    fn accept(&mut self, key: &str, text: &str) -> super::error::Result<String> {
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

    fn run_until(&mut self, done: impl Fn(&mut World) -> bool) {
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

/// The client and generation a spawned capability tool was published for.
fn published_tool(world: &mut World, tool: &str) -> Option<(String, u64)> {
    let mut query = world.query::<(&rig_ecs::tools::ToolDef, &client::CapabilityTool)>();
    query
        .iter(world)
        .find(|(definition, _)| definition.0.name.as_str() == tool)
        .map(|(_, published)| (published.client_id.clone(), published.activation_revision))
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
    harness.run_until(answered);
    harness.app.update();
    assert!(
        harness
            .history()
            .items
            .iter()
            .any(|item| item.text == "書いたよ")
    );
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
fn duplicate_capability_tool_names_are_refused_explicitly() {
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
    for id in ["c1", "c2"] {
        register(
            harness.app.world_mut(),
            id,
            capability_tool("body.motion", "body_motion"),
        );
    }
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
            if reason.contains("more than one active client")
    ));
}

#[test]
fn publishing_capability_tools_has_an_explicit_limit() {
    let mut harness = Harness::new([]);
    let count = super::client::dynamic::MAX_CAPABILITY_TOOLS + 1;
    let capabilities: Vec<String> = (0..count).map(|index| format!("cap.{index}")).collect();
    let specs: Vec<w::ClientCapabilitySpec> = (0..count)
        .map(|index| w::ClientCapabilitySpec {
            name: format!("cap.{index}"),
            tool_name: format!("cap_{index}"),
            ..Default::default()
        })
        .collect();
    client::register(
        harness.app.world_mut(),
        w::RegisterClientRequest {
            id: "c1".into(),
            name: "c1".into(),
            platform: "linux".into(),
            capabilities,
            capability_specs: specs,
            ..Default::default()
        },
    )
    .unwrap();
    let error = client::activate(
        harness.app.world_mut(),
        w::ActivationRequest {
            companion_id: harness.companion.clone(),
            client_id: "c1".into(),
            expected_revision: 0,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        super::error::HostError::Exhausted(ref reason)
            if reason.contains("capability tools can be published")
    ));
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
