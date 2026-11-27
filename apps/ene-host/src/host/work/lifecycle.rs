//! The one agent a work session keeps: spawning it, rebinding it to the run it
//! serves, retiring it, and reconciling it with the store after a restart.
use super::{RunExecutor, RunState, Runs, STALL_TIMEOUT, WorkStatus, finish_run};
use crate::host::{
    agent::{self, WorkAgent},
    db::{Result, Store},
    error::HostError,
    mind::Companion,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use std::collections::{HashMap, HashSet};
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

/// Recovers runs using their recorded executor and retains the matching agent.
pub(crate) fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let work = store.work_repo();
    let companions: HashSet<_> = store
        .companion_repo()
        .companions()?
        .into_iter()
        .map(|companion| companion.id)
        .collect();
    let restored: HashSet<_> = world
        .query::<&AgentId>()
        .iter(world)
        .filter(|agent| agent.0.starts_with("work-"))
        .map(|agent| agent.0.clone())
        .collect();
    let mut open = HashMap::new();
    for session in work.all_work_sessions()? {
        if session.status == WorkStatus::Closed {
            continue;
        }
        let mut active_id = None;
        if let Some(run) = work.active_run(&session.id)? {
            let executor = work.run_executor(&run.run_id)?.ok_or_else(|| {
                HostError::Storage("the work run disappeared during recovery".into())
            })?;
            let recoverable = match executor {
                RunExecutor::Rig => true,
                RunExecutor::Harness { .. } => false,
                RunExecutor::Unrecorded => work
                    .runs(&session.id)?
                    .iter()
                    .any(|previous| restored.contains(&agent::work_agent_id(&previous.run_id))),
            };
            if recoverable && companions.contains(session.companion_id.as_str()) {
                world.resource_mut::<Runs>().begin(&run.run_id);
                active_id = Some(run.run_id);
            } else {
                finish_run(
                    world,
                    run,
                    RunState::Failed,
                    Some(
                        "作業を続けられませんでした。ホスト再起動後に実行環境を復元できません。"
                            .into(),
                    ),
                );
            }
        }
        if companions.contains(session.companion_id.as_str()) {
            open.insert(session.id, active_id);
        }
    }
    let mut retained = HashSet::new();
    let obsolete: Vec<_> = world
        .query::<(Entity, &WorkAgent)>()
        .iter(world)
        .filter_map(|(entity, agent)| {
            let keep = open.get(&agent.work_id).is_some_and(|active| {
                active.as_ref().is_none_or(|run_id| run_id == &agent.run_id)
                    && retained.insert(agent.work_id.clone())
            });
            (!keep).then_some(entity)
        })
        .collect();
    for entity in obsolete {
        retire_entity(world, entity);
    }
    Ok(())
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
