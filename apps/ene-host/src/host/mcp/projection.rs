//! ECS tool projection for connected MCP servers.
use super::supervisor::{McpServers, Server};
use bevy_ecs::prelude::*;
use rig_agent::tool::ToolCatalog;
use rig_core::{
    effect::{Key, family},
    message::ToolName,
};
use rig_ecs::prelude::*;
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

/// The registration an MCP tool entity came from: the library generation that
/// keeps a stale refresh from replacing a newer handler, and the server that
/// owns it. The entity carries this, so no side map has to stay in step.
#[derive(Component)]
pub(super) struct McpTool {
    pub(super) server: String,
    pub(super) key: Key<family::Tool>,
}

/// Retire old owners before publishing any new tools. This keeps a retired
/// server from blocking another server that now owns the same exposed name.
pub(super) fn project_servers(world: &mut World, servers: &McpServers, retired: &[String]) {
    let obsolete: Vec<_> = world
        .query::<(Entity, &McpTool)>()
        .iter(world)
        .filter(|(_, tool)| !servers.0.contains_key(&tool.server) || retired.contains(&tool.server))
        .map(|(entity, _)| entity)
        .collect();
    let mut changed = !obsolete.is_empty();
    for entity in obsolete {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
        }
    }
    for (id, server) in &servers.0 {
        changed |= project(world, id, server, &server.registry.snapshot());
    }
    if changed {
        crate::host::tools::sync::sync_access(world);
    }
}

pub(super) fn project(world: &mut World, id: &str, server: &Server, catalog: &ToolCatalog) -> bool {
    let mut desired = HashMap::new();
    for definition in catalog.definitions() {
        let raw = definition.name.as_str();
        if !server.allowed.is_empty() && !server.allowed.iter().any(|name| name == raw) {
            continue;
        }
        let Some(exposed) = ene_work::mcp::exposed_name(&server.label, id, raw) else {
            continue;
        };
        let Ok(name) = ToolName::new(exposed.clone()) else {
            continue;
        };
        let Some(registration) = catalog.get(raw) else {
            continue;
        };
        let mut definition = (**definition).clone();
        definition.name = name;
        desired.insert(exposed, (definition, registration));
    }
    let mut current: HashMap<String, (Entity, Key<family::Tool>)> = {
        let mut query = world.query::<(Entity, &ToolDef, &McpTool)>();
        query
            .iter(world)
            .filter(|(_, _, tool)| tool.server == id)
            .map(|(entity, definition, tool)| {
                (
                    definition.0.name.as_str().to_owned(),
                    (entity, tool.key.clone()),
                )
            })
            .collect()
    };
    let mut changed = false;
    for (name, (definition, tool)) in desired {
        let existing = current.remove(&name);
        if existing.as_ref().is_some_and(|(_, key)| key == tool.key()) {
            continue;
        }
        if let Some((entity, _)) = existing {
            if let Ok(entity) = world.get_entity_mut(entity) {
                entity.despawn();
            }
            changed = true;
        }
        if let Some(entity) =
            world.spawn_tool(definition, tool.handler().clone(), ToolOptions::default())
        {
            world.entity_mut(entity).insert((
                McpTool {
                    server: id.to_owned(),
                    key: tool.key().clone(),
                },
                crate::host::resources::HostExternalTool,
            ));
            changed = true;
        }
    }
    for (_, (entity, _)) in current {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
pub(super) fn tool_names(world: &mut World) -> HashSet<String> {
    let mut query = world.query::<(&ToolDef, &McpTool)>();
    query
        .iter(world)
        .map(|(definition, _)| definition.0.name.as_str().to_owned())
        .collect()
}

pub(crate) fn tool_servers(world: &mut World) -> HashMap<String, String> {
    let mut query = world.query::<(&ToolDef, &McpTool)>();
    query
        .iter(world)
        .map(|(definition, tool)| (definition.0.name.as_str().to_owned(), tool.server.clone()))
        .collect()
}
