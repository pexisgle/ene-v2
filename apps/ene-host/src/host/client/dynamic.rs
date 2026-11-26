//! Dynamic capability tools.
//!
//! A granted capability of an active client that declares a `tool_name` is an
//! ordinary tool registered with [`World::spawn_tool`]. Several companions can
//! share one tool definition: the definition is published once when every
//! declaration of the name agrees on the model-facing description and input
//! schema, and each active companion keeps its own binding (its active client
//! and capability) behind that name. A declaration that disagrees is an
//! explicit conflict, never a silently chosen winner.
//!
//! The handler runs off the host thread and has no access to the ECS world, so
//! it forwards only the effect id, the tool name and the input. The host thread
//! drains those calls in `Update`: it finds the call's gate binding, resolves
//! the calling companion from the call entity (`ForAgent` plus
//! `CompanionAgent`), revalidates the bound client, and answers the handler
//! through a oneshot. A call whose binding is missing, whose agent is a hand,
//! or whose bound client no longer declares the capability is refused instead
//! of dispatched.
//!
//! Waiting follows the documented distinction (`docs/api.md` §クライアント能力):
//! `device_request` is result-waiting and keeps its [`Waiting`] entry until the
//! client answers; a named capability tool is immediate and answers at once
//! with the operation id and dispatch status. Its failure or non-delivery is
//! observed through `GetOperation` or the device notification.
use super::{CapabilitySpec, Client};
use crate::host::{
    client::device,
    db::{Result, Store},
    error::HostError,
    permission::CallBinding,
    resources::{CompanionAgent, GrantedCompanionTools},
};
use bevy_ecs::prelude::*;
use ene_domain::json;
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

/// How many capability calls may queue before a handler waits.
const QUEUE: usize = 64;

/// One active companion/client a shared tool dispatches for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityBinding {
    pub(crate) companion_id: String,
    pub(crate) client_id: String,
    /// The capability this client declares for the shared tool name.
    pub(crate) capability: String,
}

/// A capability tool published once for every active companion whose client
/// declares the same name, description and input schema. The model-facing
/// definition is shared; the dispatcher resolves the calling companion from
/// the call, never from this record.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityTool {
    /// The declaration the definition was built from.
    pub(crate) spec: CapabilitySpec,
    /// Every active companion/client this definition serves.
    pub(crate) bindings: Vec<CapabilityBinding>,
}

/// The capability tool entities currently published, by tool name.
#[derive(Resource, Default)]
pub(crate) struct CapabilityTools {
    pub(crate) entities: HashMap<String, Entity>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ForwardError {
    #[error("the capability call is no longer live")]
    CallNotLive,
    #[error("the capability call was not accepted by the gate")]
    NotAcceptedByGate,
    #[error("the capability call has no tool request")]
    MissingToolRequest,
    #[error("the capability call names another tool")]
    WrongToolName,
    #[error("the capability call has no agent")]
    MissingAgent,
    #[error("capability tools are offered to the parent only")]
    ParentOnly,
    #[error("the bound client is not registered")]
    ClientNotRegistered,
    #[error("capability `{0}` is no longer declared")]
    CapabilityNotDeclared(String),
    #[error("the capability no longer matches the accepted operation")]
    CapabilityMismatch,
    #[error("{0}")]
    Host(#[from] HostError),
    #[error("{0}")]
    Store(#[from] ene_store::StoreError),
}

/// One call of a capability tool, sent from its handler to the host thread.
pub(crate) struct CapabilityCall {
    /// The effect the call runs as; the host reads its gate binding from it.
    pub(crate) effect: EffectId,
    pub(crate) tool_name: String,
    pub(crate) input: Value,
    /// Sent back to the handler when the host accepted or refused the call.
    pub(crate) reply: oneshot::Sender<std::result::Result<String, ForwardError>>,
}

/// The capability call queue shared by the spawned handlers and the host
/// thread, which drains it once a frame.
#[derive(Resource)]
pub(crate) struct CapabilityCalls {
    sender: mpsc::Sender<CapabilityCall>,
    receiver: mpsc::Receiver<CapabilityCall>,
    wake: Wake,
}

impl CapabilityCalls {
    pub(crate) fn new(wake: Wake) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self {
            sender,
            receiver,
            wake,
        }
    }
}

/// A spawned capability tool's handler. It never touches the world: the call
/// it forwards names the effect and the tool, and the host thread supplies the
/// binding and the calling companion.
struct CapabilityHandler {
    name: String,
    description: String,
    parameters: Value,
    calls: mpsc::Sender<CapabilityCall>,
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
        let dispatched = self
            .calls
            .send(CapabilityCall {
                effect: dispatch.id(),
                tool_name: self.name.clone(),
                input,
                reply,
            })
            .await;
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
                Reply::Outcome(Err(
                    ErrorReport::new(ErrorKind::Denied, reason.to_string()).refused()
                ))
            }
            Err(_) => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "the host dropped the capability call before answering",
            ))),
        }
    }
}

/// The capability tools every active client grants, one definition per tool
/// name. Declarations of a name with a different description or input schema
/// are an explicit conflict.
fn desired(store: &Store) -> Result<Vec<(String, CapabilityTool)>> {
    let mut desired: Vec<(String, CapabilityTool)> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for activation in store.client_repo().activations()? {
        let Some(client_id) = activation.client_id else {
            continue;
        };
        let Some(client) = store.client_repo().client(&client_id)? else {
            continue;
        };
        for spec in client.tool_specs() {
            let name = spec.tool_name.clone();
            let parameters = json::parse_schema(&spec.input_schema)?;
            let binding = CapabilityBinding {
                companion_id: activation.companion_id.as_str().to_owned(),
                client_id: client_id.clone(),
                capability: spec.name.clone(),
            };
            match by_name.get(&name) {
                Some(&index) => {
                    let tool = &mut desired[index].1;
                    let first = &tool.spec;
                    if first.tool_description() != spec.tool_description()
                        || json::parse_schema(&first.input_schema)? != parameters
                    {
                        return Err(HostError::Conflict(format!(
                            "tool name `{name}` is declared with a different description or \
                             input schema by companions `{}` and `{}`",
                            tool.bindings[0].companion_id, activation.companion_id
                        )));
                    }
                    tool.bindings.push(binding);
                }
                None => {
                    by_name.insert(name.clone(), desired.len());
                    desired.push((
                        name,
                        CapabilityTool {
                            spec: spec.clone(),
                            bindings: vec![binding],
                        },
                    ));
                }
            }
        }
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
            .map_err(|_| HostError::Internal(("a capability tool name was empty").into()))?;
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
                    calls: calls.clone(),
                    wake: wake.clone(),
                });
                let Some(entity) = world.spawn_tool(definition, handler, ToolOptions::default())
                else {
                    rollback(world, &spawned, &stale);
                    return Err(HostError::Conflict(format!(
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
    world.resource_mut::<CapabilityTools>().entities = entities.clone();
    let granted = granted_names(world, &entities);
    world.insert_resource(GrantedCompanionTools(granted));
    crate::host::tools::sync::sync_access(world);
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

/// The capability tool names each companion's active client grants.
fn granted_names(
    world: &World,
    entities: &HashMap<String, Entity>,
) -> HashMap<String, Vec<String>> {
    let mut granted: HashMap<String, Vec<String>> = HashMap::new();
    for (name, entity) in entities {
        let Some(tool) = world.get::<CapabilityTool>(*entity) else {
            continue;
        };
        for binding in &tool.bindings {
            granted
                .entry(binding.companion_id.clone())
                .or_default()
                .push(name.clone());
        }
    }
    for names in granted.values_mut() {
        names.sort();
        names.dedup();
    }
    granted
}

/// Startup: publishes the tools of the clients restored as active.
pub(crate) fn init(world: &mut World) {
    if let Err(error) = sync_tools(world) {
        crate::host::fail(world, &error.to_string());
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
        let _discarded = call.reply.send(reply);
    }
}

/// The declaration a calling parent's bound client carries for `tool_name`,
/// when the tool is published for that client. The gate uses this before it
/// binds a capability tool call; the dispatcher uses it again before accepting
/// the operation.
pub(crate) fn tool_spec_for(
    world: &mut World,
    client: &Client,
    tool_name: &str,
) -> Result<Option<CapabilitySpec>> {
    let published = {
        let mut query = world.query::<(&ToolDef, &CapabilityTool)>();
        query
            .iter(world)
            .find(|(definition, _)| definition.0.name.as_str() == tool_name)
            .map(|(definition, tool)| (definition.0.clone(), tool.clone()))
    };
    let Some((definition, tool)) = published else {
        return Ok(None);
    };
    if !tool
        .bindings
        .iter()
        .any(|binding| binding.client_id == client.id)
    {
        return Err(HostError::Conflict(
            ("the capability tool is not published for the bound client").into(),
        ));
    }
    let Some(spec) = client
        .tool_specs()
        .find(|spec| spec.tool_name == tool_name)
        .cloned()
    else {
        return Ok(None);
    };
    let parameters = json::parse_schema(&spec.input_schema)?;
    if definition.description != spec.tool_description() || definition.parameters != parameters {
        return Err(HostError::Conflict(
            ("the capability tool changed after it was published").into(),
        ));
    }
    Ok(Some(spec))
}

fn forward(
    world: &mut World,
    request: &CapabilityCall,
) -> std::result::Result<String, ForwardError> {
    let Some(call) = effect_entity(world, request.effect) else {
        return Err(ForwardError::CallNotLive);
    };
    let Some(binding) = world.get::<CallBinding>(call).cloned() else {
        // Only the gate inserts a binding; without one the call was never
        // accepted and must not reach a client.
        return Err(ForwardError::NotAcceptedByGate);
    };
    let Some(sent) = world.get::<ToolRequest>(call) else {
        return Err(ForwardError::MissingToolRequest);
    };
    if sent.call.function.name.as_str() != request.tool_name {
        return Err(ForwardError::WrongToolName);
    }
    let Some(agent) = world.get::<ForAgent>(call).map(|agent| agent.0) else {
        return Err(ForwardError::MissingAgent);
    };
    // A capability tool is offered to the parent; a hand can only use the
    // generic `device_request` dispatcher. The calling companion comes from
    // the call's agent, not from the publication that served several
    // companions; its bound client must still be the client holding the tool.
    if world.get::<CompanionAgent>(agent).is_none() {
        return Err(ForwardError::ParentOnly);
    }
    let store = world.resource::<Store>().clone();
    let Some(client) = store.client_repo().client(&binding.client_id)? else {
        return Err(ForwardError::ClientNotRegistered);
    };
    let Some(spec) = tool_spec_for(world, &client, &request.tool_name)? else {
        return Err(ForwardError::CapabilityNotDeclared(binding.capability));
    };
    if spec.name != binding.capability {
        return Err(ForwardError::CapabilityMismatch);
    }
    // The dispatch target is the client the gate bound; a later move does not
    // change it.
    Ok(device::accept(world, call, agent, Some(&spec), request.input.clone())?.1)
}

/// The effect entity an effect id names.
fn effect_entity(world: &mut World, id: EffectId) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Effect)>();
    query
        .iter(world)
        .find(|(_, effect)| effect.id() == id)
        .map(|(entity, _)| entity)
}
