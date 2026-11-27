//! Resolving one tool call before its verdict: the saved rules it is judged
//! against, the pre-tool hooks it must pass, and the `CandidateAction` it names.
use super::approval::refuse;
use super::gate::{GateCall, GateContext};
use crate::host::{
    background::{self, AbortOnDropHandle},
    client::Client,
    db::Store,
    error::{HostError, Result},
    providers::extensions::run_hooks,
    resources::{CompanionAgent, HostExternalTool, WorkAgent, WorkTurn},
};
use bevy_ecs::prelude::*;
use ene_policy::{CandidateAction, CapabilitySpec, PolicyRule};
use ene_rpc as w;
use rig_core::effect::EffectId;
use rig_ecs::prelude::{
    CallOf, Decider, Decision, Effect, HoldOf, Outcome, SupersededBy, ToolRequest,
};
use serde_json::Value;
use tokio::sync::oneshot::{self, error::TryRecvError};

/// A work call belongs to its admitted turn, even when the agent is reused.
pub(super) fn binding_of(
    world: &mut World,
    store: &Store,
    agent: Entity,
    call: Entity,
) -> Option<(String, Option<String>, String)> {
    if let Some(companion) = world.get::<CompanionAgent>(agent) {
        return Some((companion.companion_id.clone(), None, String::new()));
    }
    let Some(work) = world.get::<WorkAgent>(agent) else {
        return Some((String::new(), None, String::new()));
    };
    let run = world
        .get::<CallOf>(call)
        .and_then(|of| world.get::<WorkTurn>(of.0));
    match store.work_repo().active_run(&work.work_id) {
        Ok(Some(active)) if run.is_some_and(|bound| bound.0 == active.run_id) => Some((
            work.companion_id.clone(),
            Some(work.work_id.clone()),
            active.run_id,
        )),
        Err(error) if error.is_storage() => {
            crate::host::fail(world, &error.to_string());
            None
        }
        _ => {
            refuse(world, call, "the work run is no longer active");
            None
        }
    }
}

pub(super) fn count_reply_calls(world: &mut World) -> std::collections::HashMap<EffectId, usize> {
    let mut reply_calls = std::collections::HashMap::new();
    let mut spawned = world.query::<(&Effect, &ToolRequest)>();
    for (effect, _) in spawned.iter(world) {
        if let Some(parent) = effect.parent {
            *reply_calls.entry(parent).or_default() += 1;
        }
    }
    reply_calls
}

pub(super) fn load_gate_rules(
    ctx: &mut GateContext,
) -> std::result::Result<Vec<PolicyRule>, ene_store::StoreError> {
    if let Some(rules) = &ctx.cached_rules {
        return Ok(rules.clone());
    }
    let rules = ctx.store.policy_repo().policy_rules()?;
    ctx.cached_rules = Some(rules.clone());
    Ok(rules)
}

#[derive(Component)]
pub(super) struct PendingToolHook {
    response: oneshot::Receiver<Option<String>>,
    _task: AbortOnDropHandle<()>,
}

#[derive(Component)]
pub(super) struct PreToolChecked;

pub(super) fn start_pre_tool_hooks(
    ctx: &GateContext,
    call: &GateCall,
    companion_id: &str,
) -> Result<Option<PendingToolHook>> {
    let Some(providers) = ctx.providers.as_ref().filter(|providers| {
        !providers
            .with_extension(ene_plugin::Extension::Hook)
            .is_empty()
    }) else {
        return Ok(None);
    };
    let runtime = ctx
        .runtime
        .as_ref()
        .ok_or_else(|| HostError::Unavailable("pre-tool hooks are unavailable".into()))?;
    let (reply, response) = oneshot::channel();
    let providers = providers.clone();
    let store = ctx.store.clone();
    let event = ene_plugin::HookEvent::PreTool {
        companion_id: companion_id.to_owned(),
        tool_name: call.tool_name.clone(),
        arguments: call.arguments.clone(),
    };
    let task = background::spawn(runtime, ctx.wake.as_ref(), async move {
        let outcome = run_hooks(&providers, &store, event).await;
        let _sent = reply.send(outcome.blocked);
    });
    Ok(Some(PendingToolHook {
        response,
        _task: task,
    }))
}

pub(super) fn collect_pre_tool_hooks(
    mut commands: Commands,
    mut hooks: Query<(Entity, &HoldOf, &mut PendingToolHook)>,
    mut calls: Query<&mut Decision, (Without<Outcome>, Without<SupersededBy>)>,
) {
    for (hold, &HoldOf(call), mut hook) in &mut hooks {
        if let Ok(mut decision) = calls.get_mut(call) {
            let reason = match hook.response.try_recv() {
                Ok(None) => {
                    commands.entity(call).insert(PreToolChecked);
                    None
                }
                Ok(Some(reason)) => Some(reason),
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Closed) => Some("pre-tool hooks did not complete".into()),
            };
            if let Some(reason) = reason {
                decision.decide(Decider::plugin("ene-host"), Outcome::refused(&reason));
            }
        }
        commands.entity(hold).despawn();
    }
}

type CandidateResolution = (CandidateAction, Option<String>, Option<String>);

pub(super) enum CandidateOutcome {
    Resolved(Box<CandidateResolution>),
    Search(Box<crate::host::web_search::PreparedSearch>),
    NeedsDeviceClient,
    Refuse(String),
}

pub(super) fn build_candidate_action(ctx: &GateContext, item: &GateCall) -> CandidateOutcome {
    if let Some(owner) = ctx.external_tools.get(&item.tool_name) {
        let tool_name = item.tool_name.clone();
        let input = item.arguments.clone();
        let action = match owner {
            HostExternalTool::Mcp { server, .. } => CandidateAction::McpTool {
                server_id: server.clone(),
                tool_name,
                input,
            },
            HostExternalTool::Plugin(plugin) => CandidateAction::PluginTool {
                plugin_id: plugin.plugin_id.clone(),
                tool_name,
                input,
            },
        };
        return CandidateOutcome::Resolved(Box::new((action, None, None)));
    }
    if item.tool_name == crate::host::web_search::NAME {
        let providers = ctx.providers.clone().unwrap_or_default();
        let resolved = match crate::host::web_search::resolve(
            &ctx.store,
            ctx.credentials.as_ref(),
            &providers,
        ) {
            Ok(resolved) => resolved,
            Err(reason) => return CandidateOutcome::Refuse(reason.to_string()),
        };
        return match resolved.prepare(&item.arguments, ctx.credentials.as_ref()) {
            Ok(search) => CandidateOutcome::Search(Box::new(search)),
            Err(reason) => CandidateOutcome::Refuse(reason.to_string()),
        };
    }
    CandidateOutcome::NeedsDeviceClient
}

pub(super) fn resolve_device_candidate(
    world: &mut World,
    store: &Store,
    companion_id: &str,
    work_id: Option<&str>,
    agent_ctx: &AgentCapabilityContext,
    item: &GateCall,
) -> Option<CandidateResolution> {
    let client_id = match store.client_repo().target(companion_id, work_id) {
        Ok(Some(client_id)) => client_id,
        Ok(None) => {
            refuse(
                world,
                item.entity,
                "no_active_client: no client is bound to this agent",
            );
            return None;
        }
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            } else {
                refuse(
                    world,
                    item.entity,
                    "client binding could not be read; failing closed",
                );
            }
            return None;
        }
    };
    let client = match store.client_repo().client(&client_id) {
        Ok(Some(client)) => client,
        Ok(None) => {
            refuse(world, item.entity, "the bound client is not registered");
            return None;
        }
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, &error.to_string());
            } else {
                refuse(
                    world,
                    item.entity,
                    "client record could not be read; failing closed",
                );
            }
            return None;
        }
    };
    let dynamic_spec = if item.tool_name != "device_request" && !agent_ctx.is_work_agent {
        crate::host::client::dynamic::tool_spec_for(&client, &item.tool_name).cloned()
    } else {
        None
    };
    let resolved = match resolve_capability(
        agent_ctx,
        dynamic_spec,
        &client,
        &item.tool_name,
        &item.arguments,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            refuse(world, item.entity, &error.to_string());
            return None;
        }
    };
    let Some((capability, spec)) = resolved else {
        refuse(
            world,
            item.entity,
            &format!("unknown device tool `{}`", item.tool_name),
        );
        return None;
    };
    if !client.is_granted(&capability) {
        refuse(
            world,
            item.entity,
            &format!("capability `{capability}` is not granted"),
        );
        return None;
    }
    let empty = serde_json::json!({});
    let input = if item.tool_name == "device_request" {
        item.arguments.get("input").unwrap_or(&empty).clone()
    } else {
        item.arguments.clone()
    };
    if let Some(spec) = &spec
        && let Err(error) = crate::host::client::validate_input(spec, &input)
    {
        refuse(world, item.entity, &error.to_string());
        return None;
    }
    Some((
        CandidateAction::DeviceCapability {
            capability: capability.clone(),
            spec: spec.map(Box::new),
            tool_name: item.tool_name.clone(),
            input,
            raw_arguments: item.arguments.clone(),
        },
        Some(client_id),
        Some(capability),
    ))
}

pub(super) struct AgentCapabilityContext {
    pub(super) is_work_agent: bool,
    pub(super) computer_model_supported: bool,
}

/// The capability a device tool call names, with its declaration.
fn resolve_capability(
    agent_ctx: &AgentCapabilityContext,
    dynamic_spec: Option<CapabilitySpec>,
    client: &Client,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<(String, Option<CapabilitySpec>)>> {
    let (capability, spec) = if tool_name == "device_request" {
        let Some(capability) = arguments.get("capability").and_then(Value::as_str) else {
            return Err(HostError::Invalid(
                ("device_request requires a capability name as a string").into(),
            ));
        };
        if !client.capabilities.iter().any(|name| name == capability) {
            return Err(HostError::Invalid(format!(
                "capability `{capability}` is not declared by the bound client"
            )));
        }
        if capability == w::COMPUTER_USE
            && (!agent_ctx.is_work_agent || !agent_ctx.computer_model_supported)
        {
            return Err(HostError::Permission(
                ("Computer Use requires an internal work agent with an image and tool capable task model").into(),
            ));
        }
        (capability.to_owned(), client.spec(capability).cloned())
    } else {
        if agent_ctx.is_work_agent {
            return Err(HostError::Permission(
                "capability tools are offered to the parent only".into(),
            ));
        }
        let Some(spec) = dynamic_spec else {
            return Ok(None);
        };
        (spec.name.clone(), Some(spec))
    };
    if capability.starts_with(crate::host::client::WORKSPACE_CAPABILITY_PREFIX)
        && !agent_ctx.is_work_agent
    {
        return Err(HostError::Permission(
            "workspace capabilities require a work session".into(),
        ));
    }
    Ok(Some((capability, spec)))
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use rig_ecs::prelude::{App, Holds, Update};

    #[tokio::test]
    async fn pre_tool_hook_holds_only_its_call_until_the_verdict() {
        let mut app = App::new();
        app.add_systems(Update, collect_pre_tool_hooks);
        let call = app.world_mut().spawn(Decision::default()).id();
        let other = app.world_mut().spawn(Decision::default()).id();
        let (reply, response) = oneshot::channel();
        app.world_mut().spawn((
            HoldOf(call),
            PendingToolHook {
                response,
                _task: AbortOnDropHandle::new(tokio::spawn(std::future::pending())),
            },
        ));

        app.update();
        assert!(app.world().get::<Holds>(call).is_some());
        assert!(app.world().get::<Holds>(other).is_none());
        reply.send(Some("blocked by hook".into())).unwrap();
        app.update();

        assert!(app.world().get::<Holds>(call).is_none());
        assert!(app.world().get::<Decision>(call).unwrap().is_decided());
        assert!(!app.world().get::<Decision>(other).unwrap().is_decided());
    }
}
