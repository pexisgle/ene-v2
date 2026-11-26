//! ECS tool projection for connected MCP servers.
use super::supervisor::{McpServers, Server};
use crate::host::resources::HostExternalTool;
use bevy_ecs::prelude::*;
use rig_agent::tool::{RegisteredTool, ToolCatalog};
use rig_core::{completion::ToolDefinition, message::ToolName};
use rig_ecs::prelude::*;
#[cfg(test)]
use std::collections::HashSet;
use std::{collections::HashMap, sync::Arc};

type ServerTools = HashMap<ToolName, (Arc<ToolDefinition>, RegisteredTool)>;
pub(super) type ToolSnapshot = HashMap<String, ServerTools>;

pub(super) fn snapshot_tools(servers: &McpServers) -> ToolSnapshot {
    servers
        .0
        .iter()
        .map(|(id, server)| {
            (
                id.clone(),
                collect_server_tools(id, server, &server.registry.snapshot()),
            )
        })
        .collect()
}

pub(super) fn collect_server_tools(
    id: &str,
    server: &Server,
    catalog: &ToolCatalog,
) -> ServerTools {
    catalog
        .definitions()
        .iter()
        .filter_map(|definition| {
            let raw = definition.name.as_str();
            if !server.allowed.is_empty() && !server.allowed.iter().any(|name| name == raw) {
                return None;
            }
            let name = ene_work::mcp::exposed_name(&server.label, id, raw)?;
            let tool = catalog.get(raw)?;
            Some((
                ToolName::new(name).ok()?,
                (Arc::clone(definition), tool.clone()),
            ))
        })
        .collect()
}

/// Keep current registrations by key and remove every obsolete owner before
/// publishing tools. A replacement can then claim the name on the same frame.
pub(super) fn project_servers(world: &mut World, mut desired: ToolSnapshot) -> bool {
    let mut replacements = Vec::new();
    let obsolete: Vec<_> = world
        .query::<(Entity, &ToolDef, &HostExternalTool)>()
        .iter(world)
        .filter_map(|(entity, definition, owner)| {
            let HostExternalTool::Mcp { server, key } = owner else {
                return None;
            };

            let name = &definition.0.name;
            if let Some((definition, tool)) =
                desired.get_mut(server).and_then(|tools| tools.remove(name))
            {
                if tool.key() == key {
                    return None;
                }
                replacements.push((server.clone(), name.clone(), (definition, tool)));
            }
            Some(entity)
        })
        .collect();
    let mut changed = !obsolete.is_empty();
    for entity in obsolete {
        let _despawned = world.try_despawn(entity);
    }
    let added = desired.into_iter().flat_map(|(server, tools)| {
        tools
            .into_iter()
            .map(move |(name, tool)| (server.clone(), name, tool))
    });
    // A refresh can reclaim its name before a colliding server is considered.
    for (server, name, (definition, tool)) in replacements.into_iter().chain(added) {
        let mut definition = Arc::unwrap_or_clone(definition);
        definition.name = name;
        if let Some(entity) =
            world.spawn_tool(definition, tool.handler().clone(), ToolOptions::default())
        {
            world.entity_mut(entity).insert(HostExternalTool::Mcp {
                server,
                key: tool.key().clone(),
            });
            changed = true;
        }
    }
    if changed {
        crate::host::tools::sync::sync_access(world);
    }
    changed
}

#[cfg(test)]
pub(super) fn tool_names(world: &mut World) -> HashSet<String> {
    let mut query = world.query::<(&ToolDef, &HostExternalTool)>();
    query
        .iter(world)
        .filter(|(_, owner)| matches!(owner, HostExternalTool::Mcp { .. }))
        .map(|(definition, _)| definition.0.name.as_str().to_owned())
        .collect()
}
