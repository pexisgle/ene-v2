//! Host-bound open tools: the agent cannot choose its owner, permission or operation id.
use std::collections::HashSet;

use bevy_ecs::system::IntoObserverSystem;
use connectrpc::ConnectError;
use rig_core::{completion::ToolDefinition, serve::ErasedHandler};
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

use super::{
    Host, Owner, Work, conversation, fail,
    store::{Result, is_storage, storage},
};
use crate::proto::chat::v1 as w;

pub(super) const CURRENT_TIME: &str = "current_time";
pub(super) const START_WORK: &str = "start_work";
pub(super) const UPDATE_STATE: &str = "update_state";
pub(super) const GUARDED_UPDATE: &str = "guarded_update";

/// Which agents a tool is offered to. The tool is registered once; its
/// access is derived, so adding a tool never edits a second list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Parent,
    Worker,
}

/// The tools registered for each role, and the access agents get from it.
#[derive(Resource, Default)]
pub(crate) struct ToolCatalog {
    parent: HashSet<String>,
    worker: HashSet<String>,
}

impl ToolCatalog {
    fn insert(&mut self, name: &str, roles: &[Role]) {
        for role in roles {
            match role {
                Role::Parent => self.parent.insert(name.to_owned()),
                Role::Worker => self.worker.insert(name.to_owned()),
            };
        }
    }

    fn names(&self, role: Role) -> Vec<String> {
        match role {
            Role::Parent => self.parent.iter().cloned().collect(),
            Role::Worker => self.worker.iter().cloned().collect(),
        }
    }
}

pub(super) fn access_for(catalog: &ToolCatalog, owner: &Owner) -> ToolAccess {
    let role = if owner.is_work() {
        Role::Worker
    } else {
        Role::Parent
    };
    ToolAccess::Only(catalog.names(role))
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NoArgs {}
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct StartWork {
    pub prompt: String,
}
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateState {
    pub value: String,
    pub expected_revision: u64,
}

impl UpdateState {
    pub(super) fn from_call(request: &ToolRequest) -> Option<Self> {
        serde_json::from_value(request.call.function.arguments_value()).ok()
    }
}

pub(super) struct ToolsPlugin;

impl Plugin for ToolsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ToolCatalog>()
            .add_systems(Update, sync_access.before(AgentLoop));
        open_tool(
            app,
            &[Role::Parent, Role::Worker],
            CURRENT_TIME,
            "Get the current Unix time in milliseconds.",
            run_tool(|_, _, _, _: NoArgs| Ok(now_ms().to_string())),
        );
        open_tool(
            app,
            &[Role::Parent],
            START_WORK,
            "Start independent work using only the supplied instructions; returns a work id immediately.",
            run_tool(|world, agent, call, args: StartWork| start(world, agent, call, args.prompt)),
        );
        open_tool(
            app,
            &[Role::Parent, Role::Worker],
            UPDATE_STATE,
            "Replace host-owned state using its expected revision; committed updates survive cancellation.",
            run_tool(|world, agent, call, args: UpdateState| {
                update(world, agent, call, args.value, args.expected_revision)
            }),
        );
        open_tool(
            app,
            &[Role::Worker],
            GUARDED_UPDATE,
            "Propose a state replacement requiring owner approval before execution.",
            run_tool(|world, agent, call, args: UpdateState| {
                update(world, agent, call, args.value, args.expected_revision)
            }),
        );
    }
}

/// Registers a host-bound open tool and records the roles it is offered to.
fn open_tool<A, M>(
    app: &mut App,
    roles: &[Role],
    name: &str,
    description: &str,
    observer: impl IntoObserverSystem<ToolCalled<A>, M>,
) where
    A: DeserializeOwned + JsonSchema + Send + Sync + 'static,
    M: 'static,
{
    app.world_mut()
        .resource_mut::<ToolCatalog>()
        .insert(name, roles);
    app.add_open_tool(name, description, ToolOptions::default(), observer);
}

/// Registers a tool found at runtime, such as an MCP server's, for the given
/// roles. Calls run through `handler` like any other tool's.
#[allow(dead_code)] // The extension point: no built-in source exists yet.
pub(crate) fn spawn_tool(
    world: &mut World,
    roles: &[Role],
    definition: ToolDefinition,
    handler: ErasedHandler,
) -> Option<Entity> {
    let name = definition.name.as_str().to_owned();
    let entity = world.spawn_tool(definition, handler, ToolOptions::default())?;
    world.resource_mut::<ToolCatalog>().insert(&name, roles);
    Some(entity)
}

/// Gives agents the tools the catalog now holds; a runtime tool is offered
/// from the next frame on.
fn sync_access(catalog: Res<ToolCatalog>, mut agents: Query<(&Owner, &mut ToolAccess)>) {
    if !catalog.is_changed() {
        return;
    }
    for (owner, mut access) in &mut agents {
        *access = access_for(&catalog, owner);
    }
}

fn run_tool<A: Clone + Send + Sync + 'static>(
    handler: fn(&mut World, Entity, &str, A) -> Result<String>,
) -> impl Fn(On<ToolCalled<A>>, Commands) {
    move |call: On<ToolCalled<A>>, mut commands: Commands| {
        let ToolCalled {
            call,
            agent,
            effect,
            args,
            ..
        } = call.event().clone();
        commands.queue(move |world: &mut World| {
            let result = handler(world, agent, &effect.to_string(), args);
            answer(world, call, result);
        });
    }
}

fn answer(world: &mut World, call: Entity, result: Result<String>) {
    let outcome = match result {
        Ok(text) => Outcome::from(ToolResult::success(text.into())),
        Err(error) => {
            if is_storage(&error) {
                fail(world, error.to_string());
            }
            Outcome::refused(
                error
                    .message
                    .unwrap_or_else(|| "host operation failed".into()),
            )
        }
    };
    if let Some(mut decision) = world.get_mut::<Decision>(call) {
        decision.decide(Decider::plugin("ene-host-test"), outcome);
    }
}

fn authorized(world: &World, agent: Entity) -> Result<w::Target> {
    world.resource::<Host>().ready()?;
    let target = world
        .get::<Owner>(agent)
        .map(|owner| owner.0.clone())
        .ok_or_else(|| ConnectError::permission_denied("unbound agent"))?;
    if !conversation::settings(world, &target.companion_id)?.tools_allowed {
        return Err(ConnectError::permission_denied("tools are not allowed"));
    }
    Ok(target)
}

fn start(world: &mut World, parent: Entity, call: &str, prompt: String) -> Result<String> {
    let owner = authorized(world, parent)?;
    if !owner.work_id.is_empty() {
        return Err(ConnectError::permission_denied(
            "workers cannot spawn workers",
        ));
    }
    if prompt.trim().is_empty() || prompt.len() > 32768 {
        return Err(ConnectError::invalid_argument("invalid work instructions"));
    }
    let parent_id = conversation::conversation(&owner);
    let operation = format!("start:{parent_id}:{call}");
    if let Some(id) = world.resource::<Host>().db.get::<String>(&operation)? {
        return Ok(id);
    }
    let id = format!("work-{}", Uuid::new_v4());
    let target = w::Target {
        companion_id: owner.companion_id.clone(),
        work_id: id.clone(),
        ..Default::default()
    };
    let (connection, binding) = conversation::connect_model(world, &target)?;
    let work = Work {
        info: w::WorkInfo {
            work_id: id.clone(),
            conversation_id: id.clone(),
            companion_id: owner.companion_id.clone(),
            backend_profile: "internal-rig".into(),
            ..Default::default()
        },
        reported: String::new(),
    };
    world
        .resource::<Host>()
        .db
        .put_pair(&format!("work:{id}"), &work, &operation, &id)?;
    let worker = conversation::spawn_agent(world, &target);
    world.entity_mut(worker).insert((connection, binding));
    // Deliberately no SpawnedBy: stopping the parent's conversation is independent.
    let author = Author::agent(parent_id, Some(RequestId(format!("work-start:{id}"))));
    world.trigger(Deliver::new(worker, prompt, DeliveryMode::Steer).with_author(author));
    conversation::changed(world, parent);
    Ok(id)
}

fn update(
    world: &mut World,
    agent: Entity,
    call: &str,
    value: String,
    expected: u64,
) -> Result<String> {
    let owner = authorized(world, agent)?;
    if value.len() > 2048 {
        return Err(ConnectError::invalid_argument(
            "state value exceeds 2048 bytes",
        ));
    }
    let key = format!("update:{}:{call}", conversation::conversation(&owner));
    let requested = serde_json::json!({"value": value, "expected": expected});
    if let Some((previous, result)) = world
        .resource::<Host>()
        .db
        .get::<(serde_json::Value, w::State)>(&key)?
    {
        if previous != requested {
            return Err(ConnectError::already_exists(
                "operation id reused with different content",
            ));
        }
        return serde_json::to_string(&result).map_err(storage);
    }
    let old = conversation::state(world, &owner.companion_id)?;
    if old.revision != expected {
        return Err(ConnectError::aborted("state revision changed"));
    }
    let result = w::State {
        value,
        revision: expected
            .checked_add(1)
            .ok_or_else(|| ConnectError::out_of_range("state revision exhausted"))?,
        ..Default::default()
    };
    world.resource::<Host>().db.put_pair(
        &format!("state:{}", owner.companion_id),
        &result,
        &key,
        &(&requested, &result),
    )?;
    conversation::changed(world, agent);
    serde_json::to_string(&result).map_err(storage)
}
