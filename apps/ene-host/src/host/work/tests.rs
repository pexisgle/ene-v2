use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
    (root, store)
}

fn session(store: &Store) -> WorkSession {
    let session = WorkSession {
        id: "w1".into(),
        companion_id: "ene".into(),
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
    store.work_repo().insert_run(&run).unwrap();
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

    settle_run(&mut world, "run-a".into(), TurnOutcome::Stopped(None));
    assert_eq!(state(&store, "run-a"), RunState::Cancelled);
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
    store
        .work_repo()
        .set_work_status("w1", WorkStatus::Active)
        .unwrap();
    restore(&mut world);
    assert_eq!(agent_of(&mut world, "w1"), Some(kept));
    assert!(world.get::<WorkAgent>(stale).is_none());

    store
        .work_repo()
        .set_work_status("w1", WorkStatus::Closed)
        .unwrap();
    restore(&mut world);
    assert!(agent_of(&mut world, "w1").is_none());
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
    assert!(runs.stalled(first_due, STALL_TIMEOUT).is_empty());
    runs.touch("run-a");
    assert!(runs.stalled(Instant::now(), STALL_TIMEOUT).is_empty());
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
        "run-a".into(),
        TurnOutcome::Answered(Message::assistant("done")),
    );
    assert_eq!(state(&store, "run-a"), RunState::Cancelled);

    run(&store, "run-b", RunState::Running);
    settle_run(
        &mut world,
        "run-b".into(),
        TurnOutcome::Answered(Message::assistant("done")),
    );
    assert_eq!(state(&store, "run-b"), RunState::Completed);
}
