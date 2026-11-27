//! ECS tool projection for connected MCP servers.
use super::{
    registration::{McpRegistration, Receiver, ServeRegisteredTool},
    supervisor::{Launch, Mcp, McpServers, Server},
};
use crate::host::{
    db::Store,
    resources::{ExternalToolOwner, HostExternalTool},
};
use bevy_ecs::prelude::*;
use rig_agent::tool::{RegisteredTool, ToolCatalog};
use rig_core::{completion::ToolDefinition, message::ToolName};
use rig_ecs::prelude::*;
#[cfg(test)]
use std::collections::HashSet;
use std::{collections::HashMap, sync::Arc};

type ServerTools = HashMap<ToolName, (Arc<ToolDefinition>, RegisteredTool)>;
pub(super) type ToolSnapshot = HashMap<String, (Launch, Receiver, ServerTools)>;

pub(super) fn snapshot_tools(servers: &McpServers) -> ToolSnapshot {
    servers
        .0
        .iter()
        .map(|(id, server)| {
            (
                id.clone(),
                (
                    server.launch.clone(),
                    server.receiver.clone(),
                    collect_server_tools(id, server, &server.registry.snapshot()),
                ),
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
            let ExternalToolOwner::Mcp(binding) = &owner.owner else {
                return None;
            };

            let name = &definition.0.name;
            if let Some((launch, receiver, (definition, tool))) = desired
                .get_mut(&binding.server)
                .and_then(|(launch, receiver, tools)| {
                    tools.remove(name).map(|tool| (&*launch, &*receiver, tool))
                })
            {
                if tool.key() == &binding.key && launch == &binding.launch {
                    return None;
                }
                replacements.push((
                    binding.server.clone(),
                    launch.clone(),
                    receiver.clone(),
                    name.clone(),
                    (definition, tool),
                ));
            }
            Some(entity)
        })
        .collect();
    let mut changed = !obsolete.is_empty();
    for entity in obsolete {
        let _despawned = world.try_despawn(entity);
    }
    let added = desired
        .into_iter()
        .flat_map(|(server, (launch, receiver, tools))| {
            tools.into_iter().map(move |(name, tool)| {
                (server.clone(), launch.clone(), receiver.clone(), name, tool)
            })
        });
    // A refresh can reclaim its name before a colliding server is considered.
    for (server, launch, receiver, name, (definition, tool)) in
        replacements.into_iter().chain(added)
    {
        let mut offered = (*definition).clone();
        offered.name = name;
        let registration = Arc::new(McpRegistration {
            server,
            launch,
            receiver,
            definition,
            key: tool.key().clone(),
            store: world.resource::<Store>().clone(),
            credentials: world.get_resource().cloned(),
            mcp: world.resource::<Mcp>().clone(),
        });
        let handler = tool
            .handler()
            .clone()
            .layered(ServeRegisteredTool(registration.clone()));
        if let Some(entity) = world.spawn_tool(offered, handler.clone(), ToolOptions::default()) {
            world.entity_mut(entity).insert(HostExternalTool {
                owner: ExternalToolOwner::Mcp(registration),
                handler,
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
        .filter(|(_, owner)| matches!(owner.owner, ExternalToolOwner::Mcp(_)))
        .map(|(definition, _)| definition.0.name.as_str().to_owned())
        .collect()
}
