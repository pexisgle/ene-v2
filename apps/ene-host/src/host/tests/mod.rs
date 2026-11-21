//! Behavior tests on a real headless app: the ECS loop, the gate, the //! conversation log and the SQLite state together. #![cfg(test)]
mod approval_tests;
mod device_tests;

use super::{
    agent, chat, client,
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
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_ecs::prelude::*;
use rig_ecs::session_log::SessionStore;
use rig_memory::DirStore;
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

pub(crate) struct Harness {
    pub(crate) app: App,
    pub(crate) companion: String,
    wakes: mpsc::Receiver<()>,
    pub(crate) root: tempfile::TempDir,
}

impl Harness {
    pub(crate) fn new(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) -> Self {
        Self::with_policy(turns, "*")
    }

    /// The default harness grants every declaration; a test that checks the
    /// policy passes the exact `ENE_HOST_ALLOWED_CAPABILITIES` value instead.
    pub(super) fn with_policy(
        turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
        policy: &str,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let (mut app, wakes) = Self::app(root.path(), turns, policy);
        Self::start(&mut app);
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
            root,
        }
    }

    /// A headless app over `state`, the same construction a real start uses.
    pub(super) fn app(
        state: &std::path::Path,
        turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
        policy: &str,
    ) -> (App, mpsc::Receiver<()>) {
        let model = MockCompletionModel::from_stream_turns(turns);
        let store = Store::open(&state.join("host.sqlite3")).unwrap();
        let logs = DirStore::open(state.join("rig")).unwrap();
        let (sender, wakes) = mpsc::channel();
        let mut app = App::new();
        app.set_error_handler(bevy_ecs::error::warn)
            .add_plugins(TaskPoolPlugin::default())
            .insert_resource(store)
            .insert_resource(super::model::Credentials::new(true))
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
            .insert_resource(super::model::ModelFactory(Arc::new(
                move |_, _, _, _, _, _| Ok(bound(&model)),
            )))
            .add_plugins((
                AgentPlugin,
                SessionLogPlugin,
                agent::AgentPlugin,
                permission::GatePlugin,
                client::ClientPlugin,
                work::WorkPlugin,
                super::mind::MindPlugin,
                super::compaction::CompactionPlugin,
                super::autonomy::AutonomyPlugin,
            ));
        app.finish();
        if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
            time.set_max_delta(Duration::from_secs(60));
        }
        (app, wakes)
    }

    /// The first frame a start runs, then the reconciliation that has to wait
    /// for it, in the order `Runtime::start_with` runs them.
    pub(super) fn start(app: &mut App) {
        app.update();
        super::restore_after_first_frame(app.world_mut()).unwrap();
    }

    /// Stops this app and starts a second one over the same state directory,
    /// as a host restart does.
    pub(crate) fn restart(&mut self, turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) {
        let (mut app, wakes) = Self::app(self.root.path(), turns, "*");
        Self::start(&mut app);
        self.app = app;
        self.wakes = wakes;
    }

    pub(crate) fn with_clock(
        turns: impl IntoIterator<Item = Vec<MockStreamEvent>>,
        clock: super::db::schedules::ScheduleClock,
    ) -> Self {
        let mut harness = Self::new(turns);
        harness.app.world_mut().insert_resource(clock);
        harness
    }

    pub(crate) fn companion_agent(&mut self) -> Entity {
        agent::agent_of(self.app.world_mut(), &self.companion).expect("the companion agent")
    }

    pub(crate) fn conversation(&self, agent: Entity) -> &Conversation {
        self.app
            .world()
            .get::<Conversation>(agent)
            .expect("an agent conversation")
    }

    /// The types of the records in the companion agent's session log, in the
    /// order it appended them.
    pub(super) fn companion_log_types(&self) -> Vec<String> {
        let name = format!("{}.jsonl", agent::companion_agent_id(&self.companion));
        let path = self.root.path().join("rig").join("logs").join(&name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("the companion's session log {name}: {error}"));
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| {
                let record: serde_json::Value =
                    serde_json::from_str(line).expect("a session log record");
                record["type"]
                    .as_str()
                    .expect("every session log record says its type")
                    .to_owned()
            })
            .collect()
    }

    pub(crate) fn tick(&mut self) {
        super::tick(&mut self.app);
    }

    pub(super) fn unbound() -> Self {
        let mut harness = Self::with_policy([], "*");
        harness
            .app
            .world_mut()
            .insert_resource(super::model::ModelFactory::default());
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
        while started.elapsed() < Duration::from_secs(10) {
            self.tick();
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

    pub(crate) fn store(&self) -> Store {
        self.app.world().resource::<Store>().clone()
    }

    /// Advances the loop until a message with exactly `text` is in the log.
    /// `answered` only says that *some* reply landed, so a turn that answers
    /// twice needs this instead of a second bare `update`.
    pub(super) fn run_until_text(&mut self, text: &str) {
        let companion = self.companion.clone();
        self.run_until(move |world| {
            chat::history(
                world,
                &w::HistoryRequestView {
                    companion_id: &companion,
                    ..Default::default()
                },
            )
            .is_ok_and(|history| history.items.iter().any(|item| item.text == text))
        });
    }

    pub(super) fn history(&mut self) -> w::History {
        chat::history(
            self.app.world_mut(),
            &w::HistoryRequestView {
                companion_id: &self.companion,
                ..Default::default()
            },
        )
        .unwrap()
    }
}

pub(crate) fn bound(model: &MockCompletionModel) -> rig_core::DynModel<Completion> {
    let spec = Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .shared();
    model.clone().priced_as(spec).into()
}

pub(super) fn answered(world: &mut World) -> bool {
    let mut query = world.query::<&Conversation>();
    query.iter(world).any(|conversation| {
        conversation
            .entries()
            .any(|entry| matches!(entry.message, Message::Assistant(_)))
    })
}

pub(crate) fn text_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// Turns for a work run that issues a gated `device_request` and then reports.
pub(super) fn work_device_turns() -> [Vec<MockStreamEvent>; 2] {
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

pub(super) fn spec(name: &str) -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        name: name.into(),
        description: format!("{name} capability"),
        approval_target: "{path}".into(),
        ..Default::default()
    }
}

/// A declared capability that always needs a Stage::Gate approval.
pub(super) fn files_capability() -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        approval: w::CapabilityApproval::Confirm.into(),
        input_schema:
            r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}"#
                .into(),
        ..spec("device.files")
    }
}

/// A granted capability published as a tool named `tool`, run without a card.
pub(super) fn capability_tool(capability: &str, tool: &str) -> w::ClientCapabilitySpec {
    w::ClientCapabilitySpec {
        tool_name: tool.into(),
        approval: w::CapabilityApproval::Auto.into(),
        ..spec(capability)
    }
}

/// The client a single-companion capability tool is published for.
pub(super) fn published_tool(world: &mut World, tool: &str) -> Option<String> {
    let mut query = world.query::<(&rig_ecs::tools::ToolDef, &client::dynamic::CapabilityTool)>();
    query
        .iter(world)
        .find(|(definition, _)| definition.0.name.as_str() == tool)
        .and_then(|(_, published)| published.bindings.first())
        .map(|binding| binding.client_id.clone())
}

/// Every active companion/client a published tool dispatches for.
pub(super) fn published_bindings(
    world: &mut World,
    tool: &str,
) -> Vec<client::dynamic::CapabilityBinding> {
    let mut query = world.query::<(&rig_ecs::tools::ToolDef, &client::dynamic::CapabilityTool)>();
    query
        .iter(world)
        .find(|(definition, _)| definition.0.name.as_str() == tool)
        .map(|(_, published)| published.bindings.clone())
        .unwrap_or_default()
}

/// A model turn that calls `tool` and ends the model call.
pub(super) fn tool_turn(id: &str, tool: &str, input: serde_json::Value) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::tool_call(id, tool, input),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// Whether any conversation holds an assistant message with exactly `text`.
pub(super) fn said(world: &mut World, text: &str) -> bool {
    let mut query = world.query::<&Conversation>();
    query.iter(world).any(|conversation| {
        conversation
            .entries()
            .any(|entry| agent::entry_text(&entry).as_deref() == Some(text))
    })
}

/// The tools a companion's parent agent may call.
pub(super) fn access_of(world: &mut World, companion: &str) -> ToolAccess {
    let entity = agent::agent_of(world, companion).expect("the companion has an agent");
    world
        .get::<ToolAccess>(entity)
        .cloned()
        .expect("a parent has a tool access")
}

/// Waits for the next pending approval and returns it.
pub(super) fn pending_approval(harness: &mut Harness) -> permission::Approval {
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

pub(super) fn register(world: &mut World, id: &str, spec: w::ClientCapabilitySpec) -> w::Client {
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

pub(super) fn activate(world: &mut World, companion: &str, client: &str) -> w::Activation {
    client::activate(
        world,
        w::ActivationRequest {
            companion_id: companion.into(),
            client_id: client.into(),
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

/// The replay and the receiver are read at one boundary: everything after
/// the cursor is in the replay, the snapshot is at its end, and the receiver
/// carries only later events.
#[test]
fn a_subscription_resumes_from_its_cursor_without_a_gap_or_a_duplicate() {
    use crate::host::notify::Notifier;
    let event = |kind: &str| {
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: kind.into(),
            ..Default::default()
        }))
    };
    fn host_event(notification: &w::Notification) -> String {
        match notification.event.as_ref() {
            Some(w::notification::Event::Generic(event)) => event.kind.clone(),
            _ => panic!("a host event"),
        }
    }
    let mut harness = Harness::new([]);
    harness.tick();
    let before = harness.app.world().resource::<Notifier>().cursor();
    harness
        .app
        .world_mut()
        .resource_mut::<Notifier>()
        .push(None, None, event("one"));
    harness.tick();
    let (snapshot, mut receiver, replayed) =
        super::subscribe(&mut harness.app, None, Some(&before)).unwrap();
    let replayed = replayed.expect("a cursor from this epoch is resumable");
    assert_eq!(replayed.len(), 1);
    assert_eq!(host_event(&replayed[0]), "one");
    let seq = replayed[0].cursor.as_option().unwrap().seq;
    assert_eq!(
        snapshot.cursor.as_option().unwrap().seq,
        seq,
        "the snapshot is at the replay's end"
    );
    assert!(
        receiver.try_recv().is_err(),
        "the receiver starts after the replayed events"
    );
    harness
        .app
        .world_mut()
        .resource_mut::<Notifier>()
        .push(None, None, event("two"));
    harness.tick();
    let next = receiver.try_recv().expect("the live event arrives");
    assert_eq!(host_event(&next), "two");
    assert_eq!(next.cursor.as_option().unwrap().seq, seq + 1);
}

#[test]
fn a_restart_restores_the_conversation_and_its_acceptance() {
    let mut harness = Harness::new([text_turn("おかえり")]);
    let accepted = harness.accept("key-1", "行ってきます").unwrap();
    harness.run_until_text("おかえり");
    assert_eq!(history_texts(&mut harness), ["行ってきます", "おかえり"]);

    harness.restart([]);
    assert_eq!(
        history_texts(&mut harness),
        ["行ってきます", "おかえり"],
        "the restored agent keeps its conversation"
    );
    assert_eq!(
        harness.accept("key-1", "行ってきます").unwrap(),
        accepted,
        "a retry binds to the restored acceptance instead of running again"
    );
    assert_eq!(history_texts(&mut harness), ["行ってきます", "おかえり"]);
    assert!(
        !published_kinds(&harness).contains(&"message_created".to_owned()),
        "a restart does not republish the restored transcript as new messages"
    );
}

/// The reply may land before the restart or not; the accepted input and its
/// receipt must survive either way. The unfinished-turn restore itself is the
/// kernel's, and the real-binary restart test covers the same input path.
#[test]
fn a_restart_keeps_an_accepted_input_and_its_acceptance() {
    let mut harness = Harness::new([text_turn("delivered")]);
    let accepted = harness.accept("key-2", "途中だけど").unwrap();
    harness.restart([]);
    assert!(
        history_texts(&mut harness)
            .iter()
            .any(|text| text == "途中だけど"),
        "the accepted input survives a restart"
    );
    assert_eq!(harness.accept("key-2", "途中だけど").unwrap(), accepted);
    let after = history_texts(&mut harness);
    assert_eq!(
        after.iter().filter(|text| *text == "途中だけど").count(),
        1,
        "the retry does not add a second input"
    );
}

/// A restart resumes the restored agent instead of replacing it, so the
/// companion's session log gains neither a retirement of that agent nor a
/// reset of its conversation.
#[test]
fn a_restart_keeps_the_conversation_and_appends_no_retire_or_reset() {
    let mut harness = Harness::new([text_turn("おかえり")]);
    harness.accept("key-1", "行ってきます").unwrap();
    harness.run_until_text("おかえり");

    harness.restart([]);
    assert_eq!(
        history_texts(&mut harness),
        ["行ってきます", "おかえり"],
        "a restart keeps both the input and its reply"
    );
    // The kernel saves a frame's changes before it writes them, so the frame
    // after the boot holds whatever the reconciliation appended.
    harness.tick();
    let types = harness.companion_log_types();
    assert!(
        types.contains(&"push".to_owned()),
        "the companion's log holds the conversation: {types:?}"
    );
    assert!(
        !types.contains(&"retired".to_owned()),
        "a restart appends no retired record: {types:?}"
    );
    assert!(
        !types.contains(&"reset".to_owned()),
        "a restart appends no reset record: {types:?}"
    );
}

/// The kinds of every notification this boot flushed.
pub(super) fn published_kinds(harness: &Harness) -> Vec<String> {
    let notifier = harness.app.world().resource::<super::notify::Notifier>();
    let cursor = w::Cursor {
        epoch: notifier.cursor().epoch.clone(),
        seq: 0,
        ..Default::default()
    };
    notifier
        .replay(&cursor)
        .unwrap_or_default()
        .iter()
        .map(|event| event.kind.clone())
        .collect()
}

pub(super) fn history_texts(harness: &mut Harness) -> Vec<String> {
    chat::history(
        harness.app.world_mut(),
        &w::HistoryRequestView {
            companion_id: &harness.companion,
            ..Default::default()
        },
    )
    .expect("the history of a companion")
    .items
    .iter()
    .map(|item| item.text.clone())
    .collect()
}

pub(crate) fn texts(conversation: &Conversation) -> Vec<String> {
    conversation
        .entries()
        .filter_map(|entry| agent::entry_text(&entry))
        .collect()
}
