use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    store.companion_repo().seed_initial_companion().unwrap();
    (root, store)
}

fn session(store: &Store) -> WorkSession {
    let session = WorkSession {
        id: "w1".into(),
        companion_id: ene_domain::CompanionId::parse("ene").unwrap(),
        title: "task".into(),
        status: WorkStatus::Active,
        client_id: None,
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.work_repo().insert_work_session(&session).unwrap();
    session
}

fn run(store: &Store, run_id: &str, state: RunState) {
    let run = new_run(run_id.into(), "w1", "do it", state);
    store
        .work_repo()
        .insert_run(&run, &RunExecutor::Rig, None)
        .unwrap();
}

fn world(store: Store) -> World {
    let mut world = World::new();
    world.insert_resource(store);
    world.insert_resource(Notifier::new());
    world.insert_resource(WorkReports::default());
    world.insert_resource(Runs::default());
    world.insert_resource(hand::Hands::default());
    world
}

fn agent(world: &mut World, run_id: &str) -> Entity {
    world
        .spawn(WorkAgent {
            work_id: "w1".into(),
            run_id: run_id.into(),
            companion_id: "ene".into(),
        })
        .id()
}

fn state(store: &Store, run_id: &str) -> RunState {
    store.work_repo().run(run_id).unwrap().unwrap().state
}

#[test]
fn a_session_keeps_one_agent_across_runs_and_retires_it_with_the_session() {
    let (_root, store) = store();
    session(&store);
    run(&store, "run-a", RunState::Running);
    let mut world = world(store.clone());
    let kept = agent(&mut world, "run-a");

    bind_work_agent(world.get_mut::<WorkAgent>(kept), "run-b");
    assert_eq!(agent_of(&mut world, "w1"), Some(kept));
    assert_eq!(world.get::<WorkAgent>(kept).unwrap().run_id, "run-b");

    settle_run(
        &mut world,
        "run-a",
        &TurnOutcome::Answered(Message::assistant("done")),
    );
    assert_eq!(state(&store, "run-a"), RunState::Completed);
    assert_eq!(
        store
            .work_repo()
            .work_session("w1")
            .unwrap()
            .unwrap()
            .status,
        WorkStatus::Idle
    );
    assert_eq!(agent_of(&mut world, "w1"), Some(kept));

    run(&store, "run-b", RunState::Running);
    let stale = agent(&mut world, "run-a");
    restore(&mut world).unwrap();
    assert_eq!(agent_of(&mut world, "w1"), Some(kept));
    assert!(world.get::<WorkAgent>(stale).is_none());

    store.work_repo().close_work("w1").unwrap();
    restore(&mut world).unwrap();
    assert!(agent_of(&mut world, "w1").is_none());
}

#[test]
fn a_rig_work_session_adopts_its_saved_conversation_before_the_next_run() {
    use crate::host::tests::Harness;
    use rig_core::test_utils::MockStreamEvent;

    let response = |text| {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    };
    let mut harness = Harness::new([response("first work answer"), response("first parent ack")]);
    let session = start(
        harness.app.world_mut(),
        &w::StartWorkRequest {
            companion_id: harness.companion.clone(),
            instruction: "first instruction".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let store = harness.store();
    let idle = || store.work_repo().active_run(&session.id).unwrap().is_none();
    harness.run_until(|_| idle());
    harness.restart([
        response("second work answer"),
        response("second parent ack"),
    ]);

    let worker = agent_of(harness.app.world_mut(), &session.id).unwrap();
    let agent_id = &harness.app.world().get::<AgentId>(worker).unwrap().0;
    assert_eq!(*agent_id, agent::work_agent_id(&session.id));
    assert!(
        harness
            .conversation(worker)
            .entries()
            .any(|entry| agent::entry_text(&entry).as_deref() == Some("first work answer"))
    );
    send(
        harness.app.world_mut(),
        &w::SendWorkRequest {
            work_id: session.id.clone(),
            request_key: "the-follow-up".into(),
            text: "second instruction".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(agent_of(harness.app.world_mut(), &session.id), Some(worker));
    let prompt = harness.app.world().get::<SystemPrompt>(worker).unwrap();
    assert!(prompt.0.contains("second instruction"));
    harness.run_until(|_| idle());
    assert!(
        harness
            .conversation(worker)
            .entries()
            .any(|entry| agent::entry_text(&entry).as_deref() == Some("second work answer"))
    );
    assert_eq!(store.work_repo().runs(&session.id).unwrap().len(), 2);

    // A crash can leave a committed Run before Rig saved its first input.
    // An earlier session transcript does not prove that new input reached Rig.
    let incomplete = new_run(
        "not-yet-delivered".into(),
        &session.id,
        "third instruction",
        RunState::Running,
    );
    store
        .work_repo()
        .insert_run(&incomplete, &RunExecutor::Rig, None)
        .unwrap();
    harness.restart([]);
    assert_eq!(state(&store, &incomplete.run_id), RunState::Failed);
    assert!(agent_of(harness.app.world_mut(), &session.id).is_some());
}

#[test]
fn a_stall_is_warned_once_and_reset_by_activity() {
    use std::time::Duration;
    let mut runs = Runs::default();
    runs.begin("run-a");
    let first_due = Instant::now() + STALL_TIMEOUT + Duration::from_secs(1);
    assert_eq!(
        runs.stalled(first_due, STALL_TIMEOUT),
        vec!["run-a".to_owned()]
    );
    assert_eq!(
        runs.stalled(first_due, STALL_TIMEOUT),
        [] as [std::string::String; 0]
    );
    runs.touch("run-a");
    assert_eq!(
        runs.stalled(Instant::now(), STALL_TIMEOUT),
        [] as [std::string::String; 0]
    );
    assert_eq!(
        runs.stalled(
            Instant::now() + STALL_TIMEOUT + Duration::from_secs(1),
            STALL_TIMEOUT
        ),
        vec!["run-a".to_owned()],
        "activity resets the silence window"
    );
}

#[test]
fn stop_then_close_settles_the_run_and_releases_the_agent() {
    let (_root, store) = store();
    session(&store);
    run(&store, "run-a", RunState::Running);
    let mut world = world(store.clone());
    let kept = agent(&mut world, "run-a");

    stop(&mut world, "w1").unwrap();
    assert_eq!(state(&store, "run-a"), RunState::Cancelled);
    assert_eq!(agent_of(&mut world, "w1"), Some(kept));

    close(&mut world, "w1").unwrap();
    assert_eq!(
        store
            .work_repo()
            .work_session("w1")
            .unwrap()
            .unwrap()
            .status,
        WorkStatus::Closed
    );
    assert!(agent_of(&mut world, "w1").is_none());
}

#[test]
fn a_completion_that_races_a_stop_stays_cancelled() {
    let (_root, store) = store();
    session(&store);
    run(&store, "run-a", RunState::Running);
    let mut world = world(store.clone());

    stop(&mut world, "w1").unwrap();
    settle_run(
        &mut world,
        "run-a",
        &TurnOutcome::Answered(Message::assistant("done")),
    );
    assert_eq!(state(&store, "run-a"), RunState::Cancelled);

    run(&store, "run-b", RunState::Running);
    settle_run(
        &mut world,
        "run-b",
        &TurnOutcome::Answered(Message::assistant("done")),
    );
    assert_eq!(state(&store, "run-b"), RunState::Completed);
}
