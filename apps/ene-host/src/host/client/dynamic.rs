//! Active client capability tools. Rig registers each alias once. Each
//! companion's requests carry the schema and description declared by its
//! active client. The host rechecks that client's current scope before dispatch.
use super::{CapabilitySpec, Client};
use crate::host::{
    client::device,
    db::{Result, Store},
    error::HostError,
    permission::CallBinding,
    resources::CompanionAgent,
};
use bevy_ecs::prelude::*;
use ene_domain::json;
use rig_core::{
    completion::ToolDefinition,
    effect::{Answered, EffectId, EffectKind, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::ToolName,
    serve::{Dispatch, ErasedHandler, Reply, Serve},
    tool::{ToolErrorKind, ToolOutput, ToolResult},
};
use rig_ecs::prelude::*;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{mpsc, oneshot};

/// How many capability calls may queue before a handler waits.
const QUEUE: usize = 64;

/// One declaration behind a registered alias.
#[derive(Clone, Debug)]
pub(crate) struct CapabilityBinding {
    pub(crate) companion_id: String,
    pub(crate) client_id: String,
    definition: Arc<ToolDefinition>,
}

#[derive(Component, Default)]
pub(crate) struct CapabilityTool {
    pub(crate) bindings: Vec<CapabilityBinding>,
}

struct CapabilityCall {
    effect: EffectId,
    tool_name: String,
    input: Value,
    reply: oneshot::Sender<Result<String>>,
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
    definition: ToolDefinition,
    calls: mpsc::Sender<CapabilityCall>,
    wake: Wake,
}

impl Serve for CapabilityHandler {
    type Family = family::Tool;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            self.definition.name.as_str(),
            self.definition.description.clone(),
            self.definition.parameters.clone(),
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
                tool_name: self.definition.name.as_str().to_owned(),
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

fn desired(store: &Store) -> Result<HashMap<String, CapabilityTool>> {
    let repo = store.client_repo();
    let clients = repo.clients()?;
    let clients: HashMap<_, _> = clients
        .iter()
        .map(|client| (client.id.as_str(), client))
        .collect();
    let mut desired: HashMap<String, CapabilityTool> = HashMap::new();
    for activation in repo.activations()? {
        let Some(client) = activation
            .client_id
            .as_deref()
            .and_then(|id| clients.get(id))
        else {
            continue;
        };
        for spec in client.tool_specs() {
            let Ok(parameters) = json::parse_schema(&spec.input_schema) else {
                tracing::warn!(tool = %spec.tool_name, "invalid stored capability schema");
                continue;
            };
            if json::check_schema(&parameters).is_err() {
                tracing::warn!(tool = %spec.tool_name, "invalid stored capability schema");
                continue;
            }
            let name = ToolName::new(spec.tool_name.as_str())
                .map_err(|_| HostError::Invalid("the capability tool name is invalid".into()))?;
            let definition = Arc::new(ToolDefinition::new(
                name,
                spec.tool_description(),
                parameters,
            ));
            desired
                .entry(spec.tool_name.clone())
                .or_default()
                .bindings
                .push(CapabilityBinding {
                    companion_id: activation.companion_id.as_str().to_owned(),
                    client_id: client.id.clone(),
                    definition,
                });
        }
    }
    Ok(desired)
}

#[derive(Clone, Copy)]
pub(crate) enum PublicationChange<'a> {
    Refresh,
    Client(&'a str),
    Companion(&'a str),
}

impl PublicationChange<'_> {
    fn affects(self, binding: &CapabilityBinding) -> bool {
        match self {
            Self::Refresh => false,
            Self::Client(id) => binding.client_id == id,
            Self::Companion(id) => binding.companion_id == id,
        }
    }
}

/// Reconciles aliases. A schema change updates the binding without replacing
/// the alias or interrupting an unrelated companion's tools.
pub(crate) fn sync_tools(world: &mut World, change: PublicationChange<'_>) -> Result<()> {
    let mut wanted = desired(world.resource::<Store>())?;
    let external: HashSet<_> = world
        .query_filtered::<&ToolDef, Without<CapabilityTool>>()
        .iter(world)
        .map(|definition| definition.0.name.as_str().to_owned())
        .collect();
    for name in external {
        if let Some(tool) = wanted.remove(&name) {
            if tool.bindings.iter().any(|binding| change.affects(binding)) {
                return Err(HostError::Conflict(format!(
                    "tool name `{name}` is already registered"
                )));
            }
            tracing::warn!(tool = %name, "capability tool conflicts with a registered tool");
        }
    }

    let mut current: HashMap<_, _> = world
        .query_filtered::<(Entity, &ToolDef), With<CapabilityTool>>()
        .iter(world)
        .map(|(entity, definition)| (definition.0.name.as_str().to_owned(), entity))
        .collect();
    let calls = world.resource::<CapabilityCalls>();
    let (sender, wake) = (calls.sender.clone(), calls.wake.clone());
    let mut added = Vec::new();
    for (name, tool) in &wanted {
        if current.contains_key(name) {
            continue;
        }
        let definition = ToolDefinition::new(
            tool.bindings[0].definition.name.clone(),
            "Invoke a capability on the active device.",
            serde_json::json!({"type": "object"}),
        );
        let handler = ErasedHandler::new(CapabilityHandler {
            definition: definition.clone(),
            calls: sender.clone(),
            wake: wake.clone(),
        });
        let Some(entity) = world.spawn_tool(definition, handler, ToolOptions::default()) else {
            for entity in added {
                let _discarded = world.try_despawn(entity);
            }
            return Err(HostError::Conflict(format!(
                "tool name `{name}` is already registered"
            )));
        };
        added.push(entity);
        current.insert(name.clone(), entity);
    }
    for (name, entity) in current {
        if let Some(tool) = wanted.remove(&name) {
            world.entity_mut(entity).insert(tool);
        } else {
            let _discarded = world.try_despawn(entity);
        }
    }
    crate::host::tools::sync::sync_access(world);
    Ok(())
}

pub(super) fn describe(
    mut requests: Query<(&mut ModelRequest, &ForAgent), device::NewTurnCall>,
    agents: Query<&CompanionAgent>,
    tools: Query<(&ToolDef, &CapabilityTool)>,
) {
    for (mut request, agent) in &mut requests {
        let Ok(companion) = agents.get(agent.0) else {
            continue;
        };
        request.request_mut().tools.retain_mut(|definition| {
            let Some((_, tool)) = tools
                .iter()
                .find(|(alias, _)| alias.0.name == definition.name)
            else {
                return true;
            };
            let Some(binding) = tool
                .bindings
                .iter()
                .find(|binding| binding.companion_id == companion.companion_id)
            else {
                return false;
            };
            *definition = Arc::clone(&binding.definition);
            true
        });
    }
}

/// Startup: publishes the tools of the clients restored as active.
pub(crate) fn init(world: &mut World) {
    if let Err(error) = sync_tools(world, PublicationChange::Refresh) {
        crate::host::fail(world, &error.to_string());
    }
}

/// Drains the capability calls the spawned handlers queued. Each one is
/// matched to its effect and refused unless the gate bound it to the client
/// generation the tool was published for.
pub(crate) fn drain(world: &mut World) {
    let mut incoming = world.resource_mut::<CapabilityCalls>();
    let calls: Vec<_> = std::iter::from_fn(|| incoming.receiver.try_recv().ok()).collect();
    for call in calls {
        forward(world, call);
    }
}

pub(crate) fn tool_spec_for<'a>(client: &'a Client, name: &str) -> Option<&'a CapabilitySpec> {
    client.tool_specs().find(|spec| spec.tool_name == name)
}

fn forward(world: &mut World, request: CapabilityCall) {
    let CapabilityCall {
        effect,
        tool_name,
        input,
        reply,
    } = request;
    let result = forward_bound(world, effect, &tool_name, input);
    if let Err(error) = &result
        && error.is_storage()
    {
        crate::host::fail(world, &error.to_string());
    }
    let _discarded = reply.send(result);
}

fn forward_bound(world: &mut World, effect: EffectId, name: &str, input: Value) -> Result<String> {
    let call = effect_entity(world, effect)
        .ok_or_else(|| HostError::Conflict("the capability call is no longer live".into()))?;
    if world.get::<CallBinding>(call).is_none() {
        return Err(HostError::Permission(
            "the gate did not accept the capability call".into(),
        ));
    }
    if !world
        .get::<ToolRequest>(call)
        .is_some_and(|sent| sent.call.function.name.as_str() == name)
    {
        return Err(HostError::Conflict(
            "the capability call no longer names this tool".into(),
        ));
    }
    let agent = world
        .get::<ForAgent>(call)
        .map(|agent| agent.0)
        .filter(|agent| world.get::<CompanionAgent>(*agent).is_some())
        .ok_or_else(|| HostError::Permission("capability tools require a companion".into()))?;
    Ok(device::accept(world, call, agent, device::Invocation::Alias(name), input)?.1)
}

/// The effect entity an effect id names.
fn effect_entity(world: &mut World, id: EffectId) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Effect)>();
    query
        .iter(world)
        .find(|(_, effect)| effect.id() == id)
        .map(|(entity, _)| entity)
}
