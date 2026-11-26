//! Synchronizes model-facing `ToolAccess` across companion and work agents.
use crate::host::{
    client::dynamic::CapabilityTool,
    resources::{CompanionAgent, HostExternalTool, WorkAgent},
    web_search,
};
use bevy_ecs::{prelude::*, schedule::common_conditions::any_match_filter};
use ene_rpc::BUILTIN_TOOLS;
use rig_ecs::prelude::*;
use std::borrow::Cow;

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
        app.add_systems(
            Update,
            update_access
                .run_if(any_match_filter::<Or<(Added<CompanionAgent>, Added<WorkAgent>)>>)
                .before(AgentLoop),
        );
    }
}

fn sorted_access<'a>(names: impl Iterator<Item = &'a str>) -> ToolAccess {
    let mut names: Vec<_> = names.collect();
    names.sort_unstable();
    ToolAccess::only(names)
}

fn parent_access(granted: &[&str]) -> ToolAccess {
    let builtins = BUILTIN_TOOLS
        .iter()
        .copied()
        .filter(|&name| name != "report");
    sorted_access(builtins.chain(granted.iter().copied()))
}

fn hand_access<'a>(external: impl Iterator<Item = &'a ToolDef>) -> ToolAccess {
    sorted_access(
        WORK_AGENT_TOOLS
            .iter()
            .copied()
            .chain(external.map(|tool| tool.0.name.as_str())),
    )
}

/// Initial access for a companion before it has an active client.
pub(crate) fn parent_tool_access() -> ToolAccess {
    parent_access(&[])
}

/// Initial access for a newly spawned or restored work agent.
pub(crate) fn hand_tool_access(world: &mut World) -> ToolAccess {
    hand_access(
        world
            .query_filtered::<&ToolDef, With<HostExternalTool>>()
            .iter(world),
    )
}

/// Refresh access in the same actor step that updates the tool registrations.
pub(crate) fn sync_access(world: &mut World) {
    if let Err(error) = world.run_system_cached(update_access) {
        crate::host::fail(world, &error.to_string());
    }
}

type ToolAgents<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static CompanionAgent>,
        Option<&'static ToolAccess>,
    ),
    Or<(With<CompanionAgent>, With<WorkAgent>)>,
>;

fn update_access(
    capabilities: Query<(&ToolDef, &CapabilityTool)>,
    external: Query<&ToolDef, With<HostExternalTool>>,
    agents: ToolAgents<'_, '_>,
    mut commands: Commands,
) {
    let hand = hand_access(external.iter());
    let mut grants = std::collections::HashMap::<&str, Vec<&str>>::new();
    for (definition, tool) in &capabilities {
        for binding in &tool.bindings {
            grants
                .entry(binding.companion_id.as_str())
                .or_default()
                .push(definition.0.name.as_str());
        }
    }
    for (entity, companion, current) in &agents {
        let access = match companion {
            Some(companion) => Cow::Owned(parent_access(
                grants
                    .get(companion.companion_id.as_str())
                    .map_or(&[], Vec::as_slice),
            )),
            None => Cow::Borrowed(&hand),
        };
        if current.and_then(ToolAccess::names) != access.names() {
            commands.entity(entity).insert(access.into_owned());
        }
    }
}
