//! Session continuity: one long-lived work agent per `WorkSession`.
//!
//! docs/design/work.md §2 is explicit that a `WorkSession` (the context) and a
//! `Run` (one prompt turn) are different things. The agent that holds the
//! conversation is therefore owned by the session, not by the run: a run only
//! rebinds [`agent::WorkAgent::run_id`] to the run it executes. The agent is
//! retired when the session closes or is gone, and on a failed start, never
//! when an ordinary run ends.
use super::WorkStatus;
use crate::host::{
    agent::{self, WorkAgent},
    db::Store,
    mind::Companion,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// Marks the work agent that belongs to a whole session.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionAgent {
    pub(crate) work_id: String,
}

/// The agent that serves `work_id`, whether it is openly marked as a session
/// agent or was restored by the generic work-agent path first.
pub(crate) fn agent_of(world: &mut World, work_id: &str) -> Option<Entity> {
    if let Some(entity) = {
        let mut query = world.query::<(Entity, &SessionAgent)>();
        query
            .iter(world)
            .find(|(_, agent)| agent.work_id == work_id)
            .map(|(entity, _)| entity)
    } {
        return Some(entity);
    }
    let mut query = world.query::<(Entity, &WorkAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.work_id == work_id)
        .map(|(entity, _)| entity)
}

/// Points the session agent at the run it executes now.
///
/// The wire contract keeps `(session_id, run_id)` on every event; rebinding the
/// component is what makes `report`, turn settlement and permission requests
/// address the active run instead of the run the agent was spawned for.
pub(crate) fn bind(world: &mut World, entity: Entity, run_id: &str) {
    if let Some(mut work) = world.get_mut::<WorkAgent>(entity) {
        work.run_id = run_id.to_owned();
    }
}

/// Returns the session agent, spawning it with the first run's identity when
/// the session does not have one yet.
pub(crate) fn ensure(
    world: &mut World,
    companion: &Companion,
    work_id: &str,
    run_id: &str,
    instruction: &str,
) -> Entity {
    if let Some(entity) = agent_of(world, work_id) {
        bind(world, entity, run_id);
        return entity;
    }
    let entity = agent::spawn_work_agent(world, companion, work_id, run_id, instruction);
    world.entity_mut(entity).insert(SessionAgent {
        work_id: work_id.to_owned(),
    });
    entity
}

/// Retires the session agent; the session is closed or no longer exists.
pub(crate) fn retire(world: &mut World, work_id: &str) {
    let entities: Vec<Entity> = {
        let mut query = world.query::<(Entity, Option<&SessionAgent>, Option<&WorkAgent>)>();
        query
            .iter(world)
            .filter(|(_, session, work)| {
                session.is_some_and(|agent| agent.work_id == work_id)
                    || work.is_some_and(|agent| agent.work_id == work_id)
            })
            .map(|(entity, _, _)| entity)
            .collect()
    };
    for entity in entities {
        retire_entity(world, entity);
    }
}

fn retire_entity(world: &mut World, entity: Entity) {
    world.trigger(Retire { entity });
    // The session-log observer despawns the agent after saving it. A host
    // without that plugin (or an agent with no saved conversation) must still
    // release the entity instead of leaving it addressable.
    if world.get_entity(entity).is_ok() {
        world.despawn(entity);
    }
}

/// Keeps exactly one agent per open session after a restart.
///
/// `agent::reconcile` (which this host does not own) already spawns one agent
/// per open run. This pass only has to mark those agents as session agents and
/// drop duplicates that the session log restore may have brought back.
pub(crate) fn restore(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let agents: Vec<(Entity, String, String)> = {
        let mut query = world.query::<(Entity, &WorkAgent)>();
        query
            .iter(world)
            .map(|(entity, agent)| (entity, agent.work_id.clone(), agent.run_id.clone()))
            .collect()
    };
    let mut grouped: HashMap<String, Vec<(Entity, String)>> = HashMap::new();
    for (entity, work_id, run_id) in agents {
        group(&mut grouped, work_id, entity, run_id);
    }
    for (work_id, candidates) in grouped {
        let session = store.work_session(&work_id).ok().flatten();
        let keep = match &session {
            Some(session) if session.status != WorkStatus::Closed => {
                let active = store
                    .active_run(&work_id)
                    .ok()
                    .flatten()
                    .map(|run| run.run_id);
                let open = candidates
                    .iter()
                    .find(|(_, run_id)| Some(run_id.as_str()) == active.as_deref())
                    .map(|(entity, _)| *entity);
                open.or_else(|| candidates.first().map(|(entity, _)| *entity))
            }
            _ => None,
        };
        for (entity, _) in &candidates {
            if Some(*entity) == keep {
                world.entity_mut(*entity).insert(SessionAgent {
                    work_id: work_id.clone(),
                });
            } else {
                retire_entity(world, *entity);
            }
        }
    }
}

fn group(
    grouped: &mut HashMap<String, Vec<(Entity, String)>>,
    work_id: String,
    entity: Entity,
    run_id: String,
) {
    grouped.entry(work_id).or_default().push((entity, run_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        agent::WorkReports,
        db::Store,
        notify::Notifier,
        work::{Run, RunActivity, RunState, WorkSession},
    };

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        (root, store)
    }

    fn session(store: &Store, status: WorkStatus) -> WorkSession {
        let session = WorkSession {
            id: "w1".into(),
            companion_id: "ene".into(),
            title: "task".into(),
            status,
            client_id: None,
            activation_revision: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        store.insert_work_session(&session).unwrap();
        session
    }

    fn run(store: &Store, run_id: &str, state: RunState) {
        store
            .insert_run(&Run {
                run_id: run_id.into(),
                work_id: "w1".into(),
                instruction: "do it".into(),
                state,
                pending_request_id: None,
                failure: None,
                cancellation_requested_at: None,
                cancellation_reason: None,
                unverified_pids: Vec::new(),
                started_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap();
    }

    #[test]
    fn restore_keeps_one_agent_per_open_session_and_retires_closed_ones() {
        let (_root, store) = store();
        session(&store, WorkStatus::Active);
        run(&store, "run-a", RunState::Running);
        run(&store, "run-b", RunState::Completed);
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        world.insert_resource(WorkReports::default());
        world.insert_resource(RunActivity::default());
        let keep = world
            .spawn(WorkAgent {
                work_id: "w1".into(),
                run_id: "run-a".into(),
                companion_id: "ene".into(),
            })
            .id();
        let stale = world
            .spawn(WorkAgent {
                work_id: "w1".into(),
                run_id: "run-b".into(),
                companion_id: "ene".into(),
            })
            .id();
        restore(&mut world);
        assert_eq!(agent_of(&mut world, "w1"), Some(keep));
        assert!(world.get::<WorkAgent>(stale).is_none() || stale == keep);
        assert!(world.get::<SessionAgent>(keep).is_some());
    }

    #[test]
    fn a_closed_session_never_keeps_an_agent() {
        let (_root, store) = store();
        session(&store, WorkStatus::Closed);
        run(&store, "run-a", RunState::Completed);
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        world.insert_resource(WorkReports::default());
        world.insert_resource(RunActivity::default());
        let entity = world
            .spawn(WorkAgent {
                work_id: "w1".into(),
                run_id: "run-a".into(),
                companion_id: "ene".into(),
            })
            .id();
        restore(&mut world);
        assert!(world.get::<WorkAgent>(entity).is_none());
    }
}
