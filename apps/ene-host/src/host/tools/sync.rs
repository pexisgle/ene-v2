//! Synchronizes model-facing `ToolAccess` across companion and work agents.
use crate::host::{
    resources::{CompanionAgent, GrantedCompanionTools, HostExternalTool, WorkAgent},
    web_search,
};
use bevy_ecs::prelude::*;
use ene_rpc::BUILTIN_TOOLS;
use rig_ecs::prelude::*;
use std::collections::HashSet;

const WORK_AGENT_TOOLS: &[&str] = &[
    "device_request",
    "mind_read",
    "mind_recall",
    "report",
    "skill_list",
    "skill_read",
    "skill_save",
    web_search::NAME,
];

/// Registers per-agent tool access synchronization when new agents spawn.
pub(crate) struct ToolsSyncPlugin;

impl Plugin for ToolsSyncPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GrantedCompanionTools>()
            .add_systems(Update, refresh.before(AgentLoop));
    }
}

/// The tools a parent agent is offered: its built-ins and the capability
/// tools its active client granted.
fn parent_names(granted: &[String]) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_TOOLS
        .iter()
        .copied()
        .filter(|name| !matches!(*name, "report" | web_search::NAME))
        .map(str::to_owned)
        .collect();
    names.extend(granted.iter().cloned());
    names.sort();
    names.dedup();
    names
}

/// The tools a work agent ("hand") is offered: its built-ins plus external tools.
fn hand_names(host: &HashSet<String>) -> Vec<String> {
    let mut names: Vec<String> = WORK_AGENT_TOOLS
        .iter()
        .copied()
        .map(str::to_owned)
        .collect();
    names.extend(host.iter().cloned());
    names.sort();
    names.dedup();
    names
}

/// The tools a parent agent can use before any capability tool is published.
/// Capability tools are added by [`sync_access`]; a work agent uses its
/// own `ToolAccess::Only`.
pub(crate) fn parent_tool_access() -> ToolAccess {
    ToolAccess::Only(parent_names(&[]))
}

/// Initial `ToolAccess` for a newly spawned or restored work agent.
pub(crate) fn hand_tool_access(world: &mut World) -> ToolAccess {
    let host = host_tool_names(world);
    ToolAccess::Only(hand_names(&host))
}

fn host_tool_names(world: &mut World) -> HashSet<String> {
    let mut query = world.query::<(&ToolDef, &HostExternalTool)>();
    query
        .iter(world)
        .map(|(definition, _)| definition.0.name.as_str().to_owned())
        .collect()
}

/// Refreshes `ToolAccess` for both parent companions and work agents.
pub(crate) fn sync_access(world: &mut World) {
    let granted = world
        .get_resource::<GrantedCompanionTools>()
        .map(|res| res.0.clone())
        .unwrap_or_default();
    let parents: Vec<(Entity, String)> = {
        let mut query = world.query::<(Entity, &CompanionAgent)>();
        query
            .iter(world)
            .map(|(entity, agent)| (entity, agent.companion_id.clone()))
            .collect()
    };
    let host = host_tool_names(world);
    for (entity, companion) in parents {
        let granted: &[String] = granted.get(&companion).map_or(&[], Vec::as_slice);
        let names = parent_names(granted);
        set_access(world, entity, &names);
    }
    let hands: Vec<Entity> = {
        let mut query = world.query_filtered::<Entity, With<WorkAgent>>();
        query.iter(world).collect()
    };
    let hand_tools = hand_names(&host);
    for entity in hands {
        set_access(world, entity, &hand_tools);
    }
}

fn set_access(world: &mut World, entity: Entity, names: &[String]) {
    let current = world.get::<ToolAccess>(entity);
    if matches!(current, Some(ToolAccess::Only(current)) if current.as_slice() == names) {
        return;
    }
    world
        .entity_mut(entity)
        .insert(ToolAccess::Only(names.to_vec()));
}

/// Corrects the tools a newly spawned agent may use before its first turn.
fn refresh(world: &mut World) {
    let added = {
        let mut query =
            world.query_filtered::<Entity, Or<(Added<CompanionAgent>, Added<WorkAgent>)>>();
        query.iter(world).next().is_some()
    };
    if added {
        sync_access(world);
    }
}
