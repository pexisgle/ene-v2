//! Tests for the heartbeat tick: the request it builds, what it promotes and
//! how user activity cancels it.
use super::*;
use crate::host::{
    agent::TestModel,
    autonomy::testing::{Harness, bound},
    compaction::Summary,
    db::memories::MemoryRecord,
    resources::PerceptionEvent,
};
use rig_core::conversation::Aside;
use rig_core::message::UserContent;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use std::sync::atomic::{AtomicBool, Ordering};

/// A harness with the tick's dependencies: a host runtime, a heartbeat model
/// and the recall resources an ordinary turn gets from `MindPlugin`.
fn harness(tick: MockCompletionModel) -> (Harness, tokio::runtime::Runtime) {
    let mut harness = Harness::new([]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime for the tick");
    harness
        .app
        .insert_resource(HostRuntime(runtime.handle().clone()));
    harness.app.insert_resource(TestModel(Arc::new({
        let model = tick.clone();
        move || bound(&model)
    })));
    harness.app.init_resource::<CompactVectorIndex>();
    (harness, runtime)
}

/// Seeds the conversation an ordinary exchange would have left, and saves it.
fn seed(harness: &mut Harness) -> Entity {
    let agent = harness.companion_agent();
    harness
        .app
        .world_mut()
        .entity_mut(agent)
        .insert(Conversation(History::from_messages([
            Message::user("I bought tea at the shop"),
            Message::assistant("いいね"),
        ])));
    harness.tick();
    agent
}

fn idle(harness: &mut Harness) {
    harness
        .app
        .world_mut()
        .resource_mut::<InactivityTracker>()
        .last_user_interaction_at = Instant::now() - IDLE - Duration::from_secs(1);
}

/// Runs one tick to its end: it starts, the runtime answers, it settles.
fn run_tick(harness: &mut Harness) {
    let before = harness.app.world().resource::<StagedTick>().last_started;
    harness.run_until(|world| {
        let tick = world.resource::<StagedTick>();
        tick.last_started != before && tick.in_flight.is_none()
    });
}

/// The conversation length and the DirStore files, for the no-change half.
fn snapshot(harness: &mut Harness, agent: Entity) -> (usize, Vec<String>, Vec<u8>) {
    let len = harness.conversation(agent).messages().len();
    let names = harness.log_names();
    let bytes = names
        .first()
        .and_then(|name| {
            std::fs::read(harness.root.path().join("rig").join("logs").join(name)).ok()
        })
        .unwrap_or_default();
    (len, names, bytes)
}

fn memory(owner: &str, content: &str, importance: u8) -> MemoryRecord {
    MemoryRecord {
        id: uuid::Uuid::new_v4().to_string(),
        owner: owner.to_owned(),
        audiences: Vec::new(),
        content: content.to_owned(),
        importance,
        occurred_at: None,
        source_context: serde_json::json!({}),
    }
}

fn turn_text(message: &Message) -> String {
    match message {
        Message::System { content } => content.clone(),
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant(_) => String::new(),
    }
}

#[test]
fn a_silent_tick_leaves_the_conversation_and_the_log_unchanged() {
    let tick = MockCompletionModel::from_turns([MockTurn::tool_call(
        "call-1",
        "noop",
        serde_json::json!({}),
    )]);
    let (mut harness, _runtime) = harness(tick);
    let agent = seed(&mut harness);
    let (len, names, bytes) = snapshot(&mut harness, agent);
    assert_eq!(names.len(), 1, "the companion's own log only");

    idle(&mut harness);
    run_tick(&mut harness);

    // Nothing reached the companion, and the append-only log did not grow.
    let (after_len, after_names, after_bytes) = snapshot(&mut harness, agent);
    assert_eq!(after_len, len);
    assert_eq!(after_names, names);
    assert_eq!(
        after_bytes, bytes,
        "a no-op tick is never written to the DirStore"
    );
}

#[test]
fn an_acting_tick_promotes_exactly_its_input_and_reply() {
    let tick = MockCompletionModel::from_turns([MockTurn::text("おはよう。そろそろ休憩しない？")]);
    let (mut harness, _runtime) = harness(tick);
    let agent = seed(&mut harness);
    let (len, names, _) = snapshot(&mut harness, agent);

    idle(&mut harness);
    run_tick(&mut harness);
    harness.tick(); // the promotion is saved at the start of the next frame

    let conversation = harness.conversation(agent);
    assert_eq!(conversation.messages().len(), len + 2);
    let entries: Vec<_> = conversation.entries().skip(len).collect();
    assert_eq!(entries.len(), 2);
    let input = agent::entry_text(&entries[0]).expect("the promoted input text");
    assert!(input.starts_with("定期Tick: "), "{input}");
    assert_eq!(
        entries[0].author.map(|author| &author.kind),
        Some(&rig_core::conversation::AuthorKind::Plugin(
            "ene-host".into()
        ))
    );
    let reply = agent::entry_text(&entries[1]).expect("the promoted reply text");
    assert_eq!(reply, "おはよう。そろそろ休憩しない？");
    assert!(entries[1].author.is_none(), "the reply is the assistant's");

    // The promotion is written once, under the companion's own log.
    //
    // The session log queues the frame's changes in `Last` and flushes them in
    // the same schedule, so a frame usually delivers it; the order between the
    // queueing and the flush is the kernel's, not the host's, so drive until
    // the write lands rather than assuming one frame is enough.
    let text = "おはよう。そろそろ休憩しない？";
    let saved = |harness: &mut Harness| {
        names.iter().any(|name| {
            std::fs::read(harness.root.path().join("rig").join("logs").join(name))
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(text))
        })
    };
    let started = Instant::now();
    while !saved(&mut harness) && started.elapsed() < Duration::from_secs(5) {
        harness.tick();
    }
    assert_eq!(harness.log_names(), names, "no temporary agent log appears");
    assert!(
        saved(&mut harness),
        "the promotion is on disk (logs: {:?}, session failure: {:?}, host failure: {:?})",
        names
            .iter()
            .map(|name| {
                let path = harness.root.path().join("rig").join("logs").join(name);
                (name.clone(), std::fs::metadata(path).map(|meta| meta.len()))
            })
            .collect::<Vec<_>>(),
        harness
            .app
            .world()
            .resource::<rig_ecs::session_log::SessionLog>()
            .failure(),
        harness
            .app
            .world()
            .resource::<crate::host::HostState>()
            .failed,
    );
}

#[test]
fn the_tail_carries_the_summary_pinned_memories_recall_and_perception() {
    let tick = MockCompletionModel::from_turns([MockTurn::tool_call(
        "call-1",
        "noop",
        serde_json::json!({}),
    )]);
    let (mut harness, _runtime) = harness(tick.clone());
    let agent = seed(&mut harness);

    let store = harness.store();
    store
        .insert_memory(memory(&harness.companion, "pinned: 毎週日曜は休む", 10))
        .unwrap();
    store
        .insert_memory(memory(
            &harness.companion,
            "we drank tea at a shop in Kyoto",
            5,
        ))
        .unwrap();
    let through = harness
        .app
        .world()
        .get::<Conversation>(agent)
        .and_then(|conversation| conversation.entries().next())
        .map(|entry| entry.id.get())
        .expect("the seeded conversation");
    harness
        .app
        .world_mut()
        .get_mut::<Conversation>(agent)
        .expect("the companion conversation")
        .add_aside(Aside::new(
            "ene.summary",
            Author::plugin("ene-host"),
            serde_json::to_value(Summary {
                text: "これまでの流れ: 紅茶の話".to_owned(),
                through_message_id: through,
            })
            .unwrap(),
        ));
    harness
        .app
        .world_mut()
        .resource_mut::<Perception>()
        .events
        .insert(
            harness.companion.clone(),
            PerceptionEvent {
                summary: "画面: エディタ".to_owned(),
            },
        );

    idle(&mut harness);
    run_tick(&mut harness);

    let requests = tick.requests();
    assert_eq!(requests.len(), 1, "the tick makes one model call");
    let sent = requests[0]
        .chat_history
        .iter()
        .map(|message| turn_text(message))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        sent.contains("これまでの流れ: 紅茶の話"),
        "the summary head is sent: {sent}"
    );
    assert!(
        sent.contains("pinned: 毎週日曜は休む"),
        "pinned memories are sent: {sent}"
    );
    assert!(
        sent.contains("we drank tea at a shop in Kyoto"),
        "automatic recall is sent: {sent}"
    );
    assert!(
        sent.contains("画面: エディタ"),
        "perception is sent: {sent}"
    );
    assert!(sent.contains("現在日時: "), "the clock is sent: {sent}");
    assert!(sent.contains("定期Tick: "), "the heartbeat prompt is last");
}

/// Sets the flag when the task it guards is dropped.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn user_activity_aborts_an_in_flight_tick() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let task = runtime.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let started = Instant::now();
    let mut world = World::new();
    world.insert_resource(InactivityTracker {
        last_user_interaction_at: started - IDLE - Duration::from_secs(1),
    });
    world.insert_resource(StagedTick {
        in_flight: Some(InFlight {
            companion: "ene".to_owned(),
            prompt: TICK.to_owned(),
            messages: 0,
            started,
            task,
            result: Arc::new(Mutex::new(None)),
        }),
        last_started: Some(started),
        next: 0,
    });
    let mut schedule = Schedule::default();
    schedule.add_systems(settle_tick);

    // Without user activity the call stays in flight.
    schedule.run(&mut world);
    assert!(world.resource::<StagedTick>().in_flight.is_some());

    // A user message resets the idle clock through `note_user_activity`.
    world.resource_mut::<InactivityTracker>().on_user_activity();
    schedule.run(&mut world);
    assert!(world.resource::<StagedTick>().in_flight.is_none());
    runtime.block_on(tokio::task::yield_now());
    assert!(
        dropped.load(Ordering::Relaxed),
        "cancellation drops the in-flight request, not only a flag"
    );
}
