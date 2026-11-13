//! Dynamic capability tools.
//!
//! A granted capability of an active client that declares a `tool_name` is an
//! ordinary tool registered with [`World::spawn_tool`]. Its handler runs off
//! the host thread and has no access to the ECS world, so it captures the
//! companion and capability it was published for and sends every call through
//! a channel. The host thread drains those calls in `Update`: it finds the
//! call's gate binding, revalidates the client generation, and answers the
//! handler through a oneshot. A call whose binding is missing or stale is
//! refused instead of dispatched.
use super::CapabilitySpec;
use crate::host::{
    agent::CompanionAgent,
    client::device,
    db::{Result, Store},
    error::HostError,
    json,
    permission::CallBinding,
};
use bevy_ecs::prelude::*;
use rig_core::{
    effect::{Answered, EffectId, EffectKind, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::ToolName,
    serve::{Dispatch, ErasedHandler, Reply, Serve},
    tool::{ToolErrorKind, ToolOutput, ToolResult},
};
use rig_ecs::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

/// How many capability tools the host publishes at once. The limit bounds the
/// tool list offered to the model; exceeding it is reported, never truncated.
pub(crate) const MAX_CAPABILITY_TOOLS: usize = 64;

/// A capability tool published for one active client generation. The captured
/// generation lets the gate and the dispatcher refuse a stale tool.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityTool {
    pub(crate) spec: CapabilitySpec,
    pub(crate) companion_id: String,
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
}

/// The capability tool entities currently published, by tool name.
#[derive(Resource, Default)]
pub(crate) struct CapabilityTools {
    pub(crate) entities: HashMap<String, Entity>,
}

/// One call of a capability tool, sent from its handler to the host thread.
pub(crate) struct CapabilityCall {
    /// The effect the call runs as; the host reads its gate binding from it.
    pub(crate) effect: EffectId,
    pub(crate) tool_name: String,
    pub(crate) companion_id: String,
    pub(crate) capability: String,
    pub(crate) input: Value,
    /// Sent back to the handler when the host accepted or refused the call.
    pub(crate) reply: oneshot::Sender<std::result::Result<String, String>>,
}

/// The capability call queue shared by the spawned handlers and the host
/// thread, which drains it once a frame.
#[derive(Resource)]
pub(crate) struct CapabilityCalls {
    sender: mpsc::UnboundedSender<CapabilityCall>,
    receiver: mpsc::UnboundedReceiver<CapabilityCall>,
    wake: Wake,
}

impl CapabilityCalls {
    pub(crate) fn new(wake: Wake) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            wake,
        }
    }
}

/// A spawned capability tool's handler. It never touches the world: the call
/// it forwards names the effect, and the host thread supplies the binding.
struct CapabilityHandler {
    name: String,
    description: String,
    parameters: Value,
    companion_id: String,
    capability: String,
    calls: mpsc::UnboundedSender<CapabilityCall>,
    wake: Wake,
}

impl Serve for CapabilityHandler {
    type Family = family::Tool;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            &self.name,
            self.description.clone(),
            self.parameters.clone(),
        )
    }

    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        let EffectKind::ToolCall { args, .. } = kind else {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "a capability tool was dispatched a non-tool effect",
            )));
        };
        let input = match serde_json::from_str::<Value>(&args) {
            Ok(input) => input,
            Err(error) => {
                return Reply::Outcome(Err(ErrorReport::new(
                    ErrorKind::Tool(ToolErrorKind::InvalidArgs),
                    format!("the capability arguments are not JSON: {error}"),
                )));
            }
        };
        let (reply, answer) = oneshot::channel();
        let dispatched = self.calls.send(CapabilityCall {
            effect: dispatch.id(),
            tool_name: self.name.clone(),
            companion_id: self.companion_id.clone(),
            capability: self.capability.clone(),
            input,
            reply,
        });
        if dispatched.is_err() {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "the host is not accepting capability calls",
            )));
        }
        // The receiver is drained by a system that only runs in a frame: ask
        // the host loop for one instead of waiting for its next timeout.
        self.wake.wake();
        match answer.await {
            Ok(Ok(text)) => Reply::Outcome(Ok(Answered::ToolResult {
                result: ToolResult::success(ToolOutput::text(text)),
            })),
            Ok(Err(reason)) => {
                Reply::Outcome(Err(ErrorReport::new(ErrorKind::Denied, reason).refused()))
            }
            Err(_) => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "the host dropped the capability call before answering",
            ))),
        }
    }
}

/// The capability tools every active client grants, in activation order.
/// Duplicate tool names and exhaustion are refused explicitly.
fn desired(store: &Store) -> Result<Vec<(String, CapabilityTool)>> {
    let mut desired = Vec::new();
    let mut names = std::collections::HashSet::new();
    for activation in store.activations()? {
        let Some(client_id) = activation.client_id else {
            continue;
        };
        let Some(client) = store.client(&client_id)? else {
            continue;
        };
        for spec in client.tool_specs() {
            if !names.insert(spec.tool_name.clone()) {
                return Err(HostError::conflict(format!(
                    "tool name `{}` is declared by more than one active client",
                    spec.tool_name
                )));
            }
            desired.push((
                spec.tool_name.clone(),
                CapabilityTool {
                    spec: spec.clone(),
                    companion_id: activation.companion_id.clone(),
                    client_id: client_id.clone(),
                    activation_revision: activation.revision,
                },
            ));
        }
    }
    if desired.len() > MAX_CAPABILITY_TOOLS {
        return Err(HostError::exhausted(format!(
            "at most {MAX_CAPABILITY_TOOLS} capability tools can be published"
        )));
    }
    Ok(desired)
}

/// Repairs the published capability tools after any declaration or activation
/// change. Stale entities are disabled during the swap and only despawned once
/// every wanted tool exists, so a failed registration leaves the old set live.
pub(crate) fn sync_tools(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let mut wanted = Vec::new();
    for (name, tool) in desired(&store)? {
        let parameters = json::parse_schema(&tool.spec.input_schema)?;
        let tool_name = ToolName::new(name.clone())
            .map_err(|_| HostError::internal("a capability tool name was empty"))?;
        wanted.push((name, tool_name, tool, parameters));
    }
    let published = world.resource::<CapabilityTools>().entities.clone();
    let mut stale = Vec::new();
    for (name, entity) in &published {
        let keep = wanted.iter().any(|(wanted_name, _, tool, _)| {
            wanted_name == name
                && world
                    .get::<CapabilityTool>(*entity)
                    .is_some_and(|current| current == tool)
        });
        if !keep && let Ok(mut current) = world.get_entity_mut(*entity) {
            current.insert(Disabled);
            stale.push(*entity);
        }
    }
    let (calls, wake) = {
        let calls = world.resource::<CapabilityCalls>();
        (calls.sender.clone(), calls.wake.clone())
    };
    let mut entities = HashMap::with_capacity(wanted.len());
    let mut spawned: Vec<Entity> = Vec::new();
    for (name, tool_name, tool, parameters) in wanted {
        let existing = published.get(&name).copied().filter(|entity| {
            !stale.contains(entity)
                && world
                    .get::<CapabilityTool>(*entity)
                    .is_some_and(|current| current == &tool)
        });
        let entity = match existing {
            Some(entity) => entity,
            None => {
                let definition = rig_core::completion::ToolDefinition::new(
                    tool_name,
                    tool.spec.tool_description(),
                    parameters.clone(),
                );
                let handler = ErasedHandler::new(CapabilityHandler {
                    name: name.clone(),
                    description: tool.spec.tool_description(),
                    parameters,
                    companion_id: tool.companion_id.clone(),
                    capability: tool.spec.name.clone(),
                    calls: calls.clone(),
                    wake: wake.clone(),
                });
                let Some(entity) = world.spawn_tool(definition, handler, ToolOptions::default())
                else {
                    rollback(world, &spawned, &stale);
                    return Err(HostError::conflict(format!(
                        "tool name `{name}` is already registered"
                    )));
                };
                world.entity_mut(entity).insert(tool);
                spawned.push(entity);
                entity
            }
        };
        entities.insert(name, entity);
    }
    for entity in stale {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
        }
    }
    world.resource_mut::<CapabilityTools>().entities = entities;
    Ok(())
}

/// Restores the registry after a failed spawn: the newly spawned tools are
/// removed and the stale ones become visible again.
fn rollback(world: &mut World, spawned: &[Entity], stale: &[Entity]) {
    for entity in spawned {
        if let Ok(entity) = world.get_entity_mut(*entity) {
            entity.despawn();
        }
    }
    for entity in stale {
        if let Ok(mut entity) = world.get_entity_mut(*entity) {
            entity.remove::<Disabled>();
        }
    }
}

/// Startup: publishes the tools of the clients restored as active.
pub(crate) fn init(world: &mut World) {
    if let Err(error) = sync_tools(world) {
        crate::host::fail(world, error.to_string());
    }
}

/// Drains the capability calls the spawned handlers queued. Each one is
/// matched to its effect and refused unless the gate bound it to the client
/// generation the tool was published for.
pub(crate) fn drain(world: &mut World) {
    let calls: Vec<CapabilityCall> =
        world.resource_scope(|_world, mut calls: Mut<CapabilityCalls>| {
            let mut calls_out = Vec::new();
            while let Ok(call) = calls.receiver.try_recv() {
                calls_out.push(call);
            }
            calls_out
        });
    for call in calls {
        let reply = forward(world, &call);
        let _ = call.reply.send(reply);
    }
}

fn forward(world: &mut World, request: &CapabilityCall) -> std::result::Result<String, String> {
    let Some(call) = effect_entity(world, request.effect) else {
        return Err("the capability call is no longer live".into());
    };
    let Some(binding) = world.get::<CallBinding>(call).cloned() else {
        // Only the gate inserts a binding; without one the call was never
        // accepted and must not reach a client.
        return Err("the capability call was not accepted by the gate".into());
    };
    let Some(sent) = world.get::<ToolRequest>(call) else {
        return Err("the capability call has no tool request".into());
    };
    if sent.call.function.name.as_str() != request.tool_name {
        return Err("the capability call names another tool".into());
    }
    if binding.capability != request.capability {
        return Err("the capability no longer matches the accepted operation".into());
    }
    let Some(agent) = world.get::<ForAgent>(call).map(|agent| agent.0) else {
        return Err("the capability call has no agent".into());
    };
    // A capability tool is offered to the parent; a hand can only use the
    // generic `device_request` dispatcher.
    match world.get::<CompanionAgent>(agent) {
        Some(companion) if companion.companion_id == request.companion_id => {}
        _ => return Err("capability tools are offered to the parent only".into()),
    }
    let store = world.resource::<Store>().clone();
    let Some(client) = store
        .client(&binding.client_id)
        .map_err(|error| error.to_string())?
    else {
        return Err("the bound client is not registered".into());
    };
    let Some(spec) = client.spec(&request.capability).cloned() else {
        return Err(format!(
            "capability `{}` is no longer declared",
            request.capability
        ));
    };
    if spec.tool_name != request.tool_name {
        return Err("the capability no longer declares this tool".into());
    }
    device::accept(world, call, agent, &spec, request.input.clone())
        .map(|(_, answer)| answer)
        .map_err(|error| error.to_string())
}

/// The effect entity an effect id names.
fn effect_entity(world: &mut World, id: EffectId) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Effect)>();
    query
        .iter(world)
        .find(|(_, effect)| effect.id() == id)
        .map(|(entity, _)| entity)
}
