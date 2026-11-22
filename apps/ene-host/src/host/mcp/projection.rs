//! ECS tool projection for connected MCP servers.
use super::supervisor::{Desired, Launch, McpServers, Server};
use crate::host::resources::HostRuntime;
use bevy_ecs::prelude::*;
use rig_agent::tool::{ToolCatalog, server::ToolServerHandle};
use rig_core::{
    effect::{Key, family},
    message::ToolName,
};
use rig_ecs::prelude::*;
use rig_rmcp::McpConnection;
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

/// The managed registry owns generations and liveness. ECS owns exposed names.
pub(super) fn attach(
    world: &mut World,
    id: &str,
    launch: Launch,
    connection: McpConnection,
    registry: ToolServerHandle,
    want: &Desired,
) {
    retire_one(world, id);
    world.resource_mut::<McpServers>().0.insert(
        id.to_owned(),
        Server {
            connection,
            launch,
            registry,
            label: want.name.clone(),
            allowed: want.allowed.clone(),
        },
    );
}

pub(super) fn project_servers(world: &mut World) {
    let ids: Vec<String> = world.resource::<McpServers>().0.keys().cloned().collect();
    let mut changed = false;
    for id in ids {
        let Some(server) = world.resource_mut::<McpServers>().0.remove(&id) else {
            continue;
        };
        let catalog = server.registry.snapshot();
        changed |= project(world, &id, &server, Some(&catalog));
        world.resource_mut::<McpServers>().0.insert(id, server);
    }
    if changed {
        refresh_access(world);
    }
}

pub(super) fn project(
    world: &mut World,
    id: &str,
    server: &Server,
    catalog: Option<&ToolCatalog>,
) -> bool {
    let mut desired = HashMap::new();
    for definition in catalog.into_iter().flat_map(ToolCatalog::definitions) {
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
        let Some(registration) = catalog.and_then(|c| c.get(raw)) else {
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

fn refresh_access(world: &mut World) {
    crate::host::tools::sync::sync_access(world);
}

/// Closes one server and removes the tools it contributed.
pub(super) fn retire_one(world: &mut World, id: &str) {
    let Some(server) = world.resource_mut::<McpServers>().0.remove(id) else {
        return;
    };
    if project(world, id, &server, None) {
        refresh_access(world);
    }
    // Closing waits for the transport, so it runs off the host thread.
    if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
        let connection = server.connection;
        runtime.spawn(async move { connection.close().await });
    }
}
