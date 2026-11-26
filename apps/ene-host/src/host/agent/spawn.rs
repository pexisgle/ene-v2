//! One agent per companion and one per open work run.
//!
//! Every lookup and spawn goes through a stable id so a restart adopts the
//! conversation the kernel restored instead of starting a second one, and
//! [`reconcile`] retires the agents whose companion or run is gone.

use super::binding::{ensure_model, notice_model_unavailable};
use super::companion_of;
use crate::host::{
    db::{Result, Store},
    mind::Companion,
    notify::Notifier,
    resources::{CompanionAgent, LastProjectedEntry, WorkAgent},
};
use bevy_ecs::prelude::*;
use ene_host_core::prompt::{system_prompt, work_prompt};
use rig_ecs::model::Connection as ModelConnection;
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// The last summary a work run reported through `report`.
#[derive(Resource, Default)]
pub(crate) struct WorkReports {
    pub(crate) completed: HashMap<String, String>,
    /// A failure the hand reported explicitly; the settle then records Failed.
    pub(crate) failed: HashMap<String, String>,
}

/// The id rig-ecs gives the agent, and the name of its session log. It has to
/// be a name Windows accepts: a colon there is an alternate data stream, so
/// `companion:x.jsonl` silently writes to a stream of a file named
/// `companion` and the log reads back empty.
pub(crate) fn companion_agent_id(companion: &str) -> String {
    format!("companion-{companion}")
}

pub(crate) fn work_agent_id(run: &str) -> String {
    format!("work-{run}")
}

pub(crate) fn agent_of(world: &mut World, companion: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &CompanionAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.companion_id == companion)
        .map(|(entity, _)| entity)
}

pub(crate) fn work_agent_of(world: &mut World, run: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &WorkAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.run_id == run)
        .map(|(entity, _)| entity)
}

/// The entity the kernel restored under `id`, which carries the saved
/// conversation but none of the host's role components.
fn restored_agent(world: &mut World, id: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &AgentId)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.0 == id)
        .map(|(entity, _)| entity)
}

/// The kernel's saved component that says which entries were published, set
/// to the restored conversation's last entry so a restart does not republish
/// the transcript as new messages.
fn published_through(conversation: Option<&Conversation>) -> LastProjectedEntry {
    LastProjectedEntry(conversation.and_then(|conversation| {
        conversation
            .entries()
            .next_back()
            .map(|entry| entry.id.get())
    }))
}

/// Spawns the agent of a companion if it does not exist yet, or adopts the
/// one the kernel restored under the companion's stable id.
#[tracing::instrument(
    name = "agent.spawn_companion",
    skip_all,
    fields(companion_id = %companion.id)
)]
pub(crate) fn spawn_companion(world: &mut World, companion: &Companion) -> Entity {
    if let Some(entity) = agent_of(world, &companion.id) {
        return entity;
    }
    let id = companion_agent_id(&companion.id);
    let entity = match restored_agent(world, &id) {
        Some(entity) => entity,
        None => world
            .spawn((
                Name::new(format!("companion:{}", companion.id)),
                Agent,
                AgentId(id),
            ))
            .id(),
    };
    let published = published_through(world.get::<Conversation>(entity));
    world.entity_mut(entity).insert((
        CompanionAgent {
            companion_id: companion.id.clone(),
        },
        SystemPrompt(system_prompt(companion)),
        crate::host::tools::sync::parent_tool_access(),
    ));
    world.entity_mut(entity).insert(published);
    entity
}

#[tracing::instrument(
    name = "agent.spawn_work",
    skip_all,
    fields(companion_id = %companion.id, work_id = %work_id, run_id = %run_id)
)]
pub(crate) fn spawn_work_agent(
    world: &mut World,
    companion: &Companion,
    work_id: &str,
    run_id: &str,
    instruction: &str,
) -> Entity {
    if let Some(entity) = work_agent_of(world, run_id) {
        return entity;
    }
    let id = work_agent_id(run_id);
    let entity = match restored_agent(world, &id) {
        Some(entity) => entity,
        None => world
            .spawn((Name::new(format!("work:{run_id}")), Agent, AgentId(id)))
            .id(),
    };
    let published = published_through(world.get::<Conversation>(entity));
    let access = crate::host::tools::sync::hand_tool_access(world);
    world.entity_mut(entity).insert((
        WorkAgent {
            work_id: work_id.to_owned(),
            run_id: run_id.to_owned(),
            companion_id: companion.id.clone(),
        },
        SystemPrompt(work_prompt(companion, instruction)),
        access,
    ));
    world.entity_mut(entity).insert(published);
    entity
}

/// Rebuilds the system prompt after a companion edit, and drops the agent when
/// the companion is gone.
pub(crate) fn sync_companion(world: &mut World, companion_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let entity = agent_of(world, companion_id);
    match store.companion_repo().companion(companion_id)? {
        Some(companion) => {
            let entity = entity.unwrap_or_else(|| spawn_companion(world, &companion));
            world
                .entity_mut(entity)
                .insert(SystemPrompt(system_prompt(&companion)));
            world.entity_mut(entity).remove::<ModelConnection>();
            // The store is the source of truth and the binding is derived, so a
            // model the host cannot bind yet is a notice: a turn, an edit or the
            // next start retries it. A storage failure still fails.
            match ensure_model(world, entity) {
                Ok(()) => Ok(()),
                Err(error) if error.is_storage() => Err(error),
                Err(error) => {
                    notice_model_unavailable(
                        &mut world.resource_mut::<Notifier>(),
                        companion_id,
                        &error,
                    );
                    Ok(())
                }
            }
        }
        None => {
            if let Some(entity) = entity {
                world.trigger(Retire { entity });
            }
            Ok(())
        }
    }
}

/// Reconciliation after the first frame: one agent per companion, one per open
/// run, and no agent whose companion or run is gone. A store failure ends the
/// start, because read as an empty list it would retire every restored agent.
pub(crate) fn reconcile(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let companions = store.companion_repo().companions()?;
    let existing: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<CompanionAgent>, With<WorkAgent>)>>()
        .iter(world)
        .collect();
    for companion in &companions {
        spawn_companion(world, companion);
        // Work agents for runs that are still open.
        for session in store.work_repo().work_sessions(&companion.id)? {
            if let Some(run) = store.work_repo().active_run(&session.id)? {
                spawn_work_agent(world, companion, &session.id, &run.run_id, &run.instruction);
            }
        }
    }
    // Retire the kernel's default agent and any agent without a role.
    let stray: Vec<Entity> = world
        .query_filtered::<Entity, (With<Agent>, Without<CompanionAgent>, Without<WorkAgent>)>()
        .iter(world)
        .collect();
    for entity in stray {
        world.trigger(Retire { entity });
    }
    // Retire agents whose run closed or whose companion disappeared.
    for entity in existing {
        let keep = world.get::<CompanionAgent>(entity).is_some_and(|agent| {
            store
                .companion_repo()
                .companion(&agent.companion_id)
                .ok()
                .flatten()
                .is_some()
        }) || world.get::<WorkAgent>(entity).is_some_and(|agent| {
            store
                .work_repo()
                .run(&agent.run_id)
                .ok()
                .flatten()
                .is_some_and(|run| crate::host::db::work::open_state(run.state))
        });
        if !keep {
            world.trigger(Retire { entity });
        }
    }
    // Bind every remaining agent's model; a missing model is a notice, not a
    // startup failure.
    let agents: Vec<(Entity, String)> = {
        let mut query = world.query::<(Entity, Option<&CompanionAgent>, Option<&WorkAgent>)>();
        query
            .iter(world)
            .filter_map(|(entity, companion, work)| {
                companion_of(companion, work).map(|companion| (entity, companion.to_owned()))
            })
            .collect()
    };
    for (entity, companion) in agents {
        if let Err(error) = ensure_model(world, entity) {
            notice_model_unavailable(&mut world.resource_mut::<Notifier>(), &companion, &error);
        }
    }
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(None, "host_ready", "the host restored its state");
    Ok(())
}
