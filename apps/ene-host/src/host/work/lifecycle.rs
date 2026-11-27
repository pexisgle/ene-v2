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
            world
                .entity_mut(entity)
                .insert(SystemPrompt(ene_mind::prompt::work_prompt(
                    companion,
                    instruction,
                )));
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
    let companions: HashMap<_, _> = store
        .companion_repo()
        .companions()?
        .into_iter()
        .map(|companion| (companion.id.clone(), companion))
        .collect();
    let logs: HashMap<_, _> = world
        .query::<(Entity, &AgentId)>()
        .iter(world)
        .filter(|(_, id)| id.0.starts_with("work-"))
        .map(|(entity, id)| (id.0.clone(), entity))
        .collect();
    let attached: Vec<_> = world
        .query::<(Entity, &WorkAgent)>()
        .iter(world)
        .map(|(entity, worker)| (entity, worker.clone()))
        .collect();
    let mut retained = HashSet::new();
    for session in work.all_work_sessions()? {
        if session.status == WorkStatus::Closed {
            continue;
        }
        let companion = companions.get(session.companion_id.as_str());
        let active = work.active_run(&session.id)?;
        let stable = logs.get(&agent::work_agent_id(&session.id));
        let history = if stable.is_none() || active.is_none() {
            work.runs(&session.id)?
        } else {
            Vec::new()
        };
        let restored = stable
            .or_else(|| {
                history
                    .iter()
                    .rev()
                    .find_map(|run| logs.get(&agent::work_agent_id(&run.run_id)))
            })
            .copied();
        let already_attached = attached
            .iter()
            .find(|(_, worker)| {
                worker.work_id == session.id
                    && active
                        .as_ref()
                        .is_none_or(|run| run.run_id == worker.run_id)
            })
            .map(|(entity, _)| *entity);
        let recoverable = if let Some(run) = &active {
            let executor = work.run_executor(&run.run_id)?.ok_or_else(|| {
                HostError::Storage("the work run disappeared during recovery".into())
            })?;
            let legacy_run_log = restored.is_some_and(|entity| {
                logs.get(&agent::work_agent_id(&run.run_id)) == Some(&entity)
            });
            let available = restored
                .or(already_attached)
                .and_then(|entity| {
                    let agent = world.get_entity(entity).ok()?;
                    let turn = world.get_entity(agent.get::<ActiveTurn>()?.turn()).ok()?;
                    Some(is_live_run(&agent, &turn, run, &executor, legacy_run_log))
                })
                .unwrap_or(false);
            if available && companion.is_some() {
                world.resource_mut::<Runs>().begin(&run.run_id);
                true
            } else {
                finish_run(
                    world,
                    run.clone(),
                    RunState::Failed,
                    Some(
                        "ホストの再起動で作業が中断されました。実行中のターンを確認できません。"
                            .into(),
                    ),
                );
                false
            }
        } else {
            false
        };
        if let Some(companion) = companion {
            if let Some(entity) = restored
                && let Some(run) = active.as_ref().or_else(|| history.last())
            {
                if !recoverable {
                    world.trigger(StopTurn::plugin(
                        entity,
                        "ene-host",
                        "the work run is not active",
                    ));
                }
                agent::attach_work_agent(
                    world,
                    entity,
                    companion,
                    &session.id,
                    &run.run_id,
                    &run.instruction,
                );
                retained.insert(entity);
            } else if let Some(entity) = already_attached {
                retained.insert(entity);
            }
        }
    }
    for (entity, _) in attached {
        if !retained.contains(&entity) {
            retire_entity(world, entity);
        }
    }
    Ok(())
}

fn is_live_run(
    agent: &EntityRef<'_>,
    turn: &EntityRef<'_>,
    run: &super::Run,
    executor: &RunExecutor,
    legacy_run_log: bool,
) -> bool {
    if matches!(executor, RunExecutor::Harness { .. }) {
        return false;
    }
    if turn.contains::<TurnOutcome>() || turn.contains::<rig_ecs::restore::Restored>() {
        return false;
    }
    legacy_run_log
        || matches!(executor, RunExecutor::Rig)
            && turn
                .get::<Turn>()
                .and_then(|turn| agent.get::<Conversation>()?.request(turn.start))
                .is_some_and(|entry| {
                    entry.is_users_own()
                        && entry
                            .author
                            .and_then(|author| author.request.as_ref())
                            .is_some_and(|id| id.0 == run.run_id)
                })
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
