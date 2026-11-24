//! The one agent a work session keeps: spawning it, rebinding it to the run it
//! serves, retiring it, and reconciling it with the store after a restart.
use super::{RunState, Runs, STALL_TIMEOUT, WorkStatus, hand};
use crate::host::{
    agent::{self, WorkAgent},
    db::Store,
    mind::Companion,
    providers::Providers,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use std::collections::HashMap;
use std::time::Instant;

pub(super) fn agent_of(world: &mut World, work_id: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &WorkAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.work_id == work_id)
        .map(|(entity, _)| entity)
}

/// Rebinding keeps `report`, settlement and permissions on the active run.
pub(super) fn bind_work_agent(work: Option<Mut<WorkAgent>>, run_id: &str) {
    if let Some(mut work) = work {
        work.run_id = run_id.to_owned();
    }
}

pub(super) fn ensure(
    world: &mut World,
    companion: &Companion,
    work_id: &str,
    run_id: &str,
    instruction: &str,
) -> Entity {
    match agent_of(world, work_id) {
        Some(entity) => {
            bind_work_agent(world.get_mut::<WorkAgent>(entity), run_id);
            entity
        }
        None => agent::spawn_work_agent(world, companion, work_id, run_id, instruction),
    }
}

pub(super) fn retire_entity(world: &mut World, entity: Entity) {
    world.trigger(Retire { entity });
    // The session-log observer despawns the agent after saving it. A host
    // without that plugin must still release the entity.
    if world.get_entity(entity).is_ok() {
        world.despawn(entity);
    }
}

pub(super) fn retire(world: &mut World, work_id: &str) {
    let entities: Vec<Entity> = {
        let mut query = world.query::<(Entity, &WorkAgent)>();
        query
            .iter(world)
            .filter(|(_, agent)| agent.work_id == work_id)
            .map(|(entity, _)| entity)
            .collect()
    };
    for entity in entities {
        retire_entity(world, entity);
    }
}

/// Keeps one agent per open session after a restart and restarts the stall
/// clock of each open run. The active run's agent (else the first) wins.
pub(super) fn restore(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let agents: Vec<(Entity, String, String)> = {
        let mut query = world.query::<(Entity, &WorkAgent)>();
        query
            .iter(world)
            .map(|(entity, agent)| (entity, agent.work_id.clone(), agent.run_id.clone()))
            .collect()
    };
    let mut kept: HashMap<String, Entity> = HashMap::new();
    for (entity, work_id, run_id) in &agents {
        let active = match store.work_repo().active_run(work_id) {
            Ok(active) => active.map(|run| run.run_id),
            Err(error) => {
                if error.is_storage() {
                    super::super::fail(world, error.to_string());
                    return;
                }
                None
            }
        };
        let open = match store.work_repo().work_session(work_id) {
            Ok(session) => session.is_some_and(|session| session.status != WorkStatus::Closed),
            Err(error) => {
                if error.is_storage() {
                    super::super::fail(world, error.to_string());
                    return;
                }
                false
            }
        };
        if open && (active.as_deref() == Some(run_id.as_str()) || !kept.contains_key(work_id)) {
            kept.insert(work_id.clone(), *entity);
        }
    }
    for (entity, work_id, _) in agents {
        if kept.get(&work_id) != Some(&entity) {
            retire_entity(world, entity);
        }
    }
    let companions = match store.companion_repo().companions() {
        Ok(companions) => companions,
        Err(error) => {
            if error.is_storage() {
                super::super::fail(world, error.to_string());
            }
            return;
        }
    };
    for companion in companions {
        let plugin =
            hand::selected(&store, world.get_resource::<Providers>(), &companion).is_some();
        let sessions = match store.work_repo().work_sessions(&companion.id) {
            Ok(sessions) => sessions,
            Err(error) => {
                if error.is_storage() {
                    super::super::fail(world, error.to_string());
                    return;
                }
                continue;
            }
        };
        for session in sessions {
            match store.work_repo().active_run(&session.id) {
                Ok(Some(run)) => {
                    if plugin {
                        // A restart lost the harness guest's session state, so an
                        // open plugin-hand run settles failed instead of keeping
                        // a stall clock that can never tick again.
                        hand::settle_restarts(world, run);
                    } else {
                        world.resource_mut::<Runs>().begin(&run.run_id);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if error.is_storage() {
                        super::super::fail(world, error.to_string());
                        return;
                    }
                }
            }
        }
    }
}

pub(super) fn detect_stalls(world: &mut World) {
    let due = world
        .resource_mut::<Runs>()
        .stalled(Instant::now(), STALL_TIMEOUT);
    for run_id in due {
        let store = world.resource::<Store>().clone();
        let Ok(Some(run)) = store.work_repo().run(&run_id) else {
            continue;
        };
        if run.state != RunState::Running {
            continue;
        }
        let Ok(Some(session)) = store.work_repo().work_session(&run.work_id) else {
            continue;
        };
        if session.status == WorkStatus::Closed {
            continue;
        }
        let Some(parent) = agent::agent_of(world, session.companion_id.as_str()) else {
            continue;
        };
        let notice = format!(
            "[System Notification: 作業「手」(run_id: {run_id}) からの報告が90秒間途絶しています。\
             現在の状況を確認してください。]"
        );
        world.trigger(
            Deliver::new(parent, notice, DeliveryMode::Inform)
                .with_author(Author::plugin("ene-host")),
        );
    }
}
