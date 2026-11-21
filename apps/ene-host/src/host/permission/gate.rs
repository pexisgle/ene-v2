//! The permission gate (`Stage::Gate`): limits, hooks, rule evaluation, and holds.
use super::approval::{expire_orphaned_approvals, refuse, resolve_approval};
use super::decision::{
    DECISION_ARGUMENT_CHARS, DecisionInputs, Decisions, Pending, collect_decisions, push_card,
    request_context, start_decision,
};
use crate::host::{
    client::Client,
    db::Store,
    error::{HostError, Result},
    notify::Notifier,
    resources::{CompanionAgent, WorkAgent},
    tools::run_tool,
    util::truncate,
};
use bevy_ecs::prelude::*;
use ene_policy::{
    Approval, ApprovalStatus, CandidateAction, CapabilitySpec, PolicyRule, Verdict, canonicalise,
    decide_action, is_host_tool,
};
use ene_rpc as w;
use rig_core::effect::EffectId;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// The most tool calls one model reply may ask for. The old plugin contract
/// enforced this per reply; the gate is the first post-decode boundary where
/// the reply's calls are visible together.
const MAX_REPLY_TOOL_CALLS: usize = 64;
/// The most bytes an id or name in a tool call may carry.
pub(super) const MAX_TOOL_IDENTIFIER_BYTES: usize = 256;

fn within_tool_limits(id: &str, name: &str, reply_calls: usize) -> bool {
    id.len() <= MAX_TOOL_IDENTIFIER_BYTES
        && name.len() <= MAX_TOOL_IDENTIFIER_BYTES
        && reply_calls <= MAX_REPLY_TOOL_CALLS
}

/// The arguments of the head's `resolve_approval` tool
/// (docs/design/permissions.md §4.2).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ResolveApprovalArgs {
    /// The `request_id` of a pending approval of this companion.
    pub(crate) request_id: String,
    /// `approve` or `deny`.
    pub(crate) decision: String,
}

/// The marker the host inserts on a call whose approval was granted.
#[derive(Component)]
pub(crate) struct Approved;

/// What the call was accepted against. The device executor dispatches to it.
#[derive(Component, Clone, Debug)]
pub(crate) struct CallBinding {
    pub(crate) client_id: String,
    pub(crate) capability: String,
    pub(crate) computer_run: Option<String>,
}

/// A held tool call and the marker that keeps it from running.
#[derive(Clone, Copy)]
pub(crate) struct Hold {
    pub(crate) call: Entity,
    pub(crate) hold: Entity,
}

#[derive(Clone, Debug)]
pub(crate) struct SubmitOutcome {
    pub(crate) approval: Approval,
    pub(crate) rule_saved: bool,
    pub(crate) rule_error: Option<String>,
}

/// The approvals that are currently attached to a live tool call.
#[derive(Resource, Default)]
pub(crate) struct Approvals {
    pub(crate) by_request: HashMap<String, Hold>,
}

/// The saved policy rules and the gate system.
pub(crate) struct GatePlugin;

impl Plugin for GatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Decisions>()
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(
                Update,
                (expire_orphaned_approvals, collect_decisions)
                    .chain()
                    .after(AgentLoop),
            )
            .add_open_tool::<ResolveApprovalArgs, _>(
                "resolve_approval",
                "Approve or deny one pending approval request of this companion. Call it when \
                 the user has agreed or refused in conversation to the operation the approval \
                 card names. Arguments: request_id, decision (`approve` or `deny`).",
                ToolOptions::default(),
                run_tool(resolve_approval),
            );
    }
}

/// Applies the decision pipeline to every tool call that is about to start.
/// Internal host tools pass; device capabilities, MCP tools, and web_search
/// enter `Stage::Gate`, are checked against saved rules and, when needed, held
/// for Decision API evaluation or user approval.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn gate(world: &mut World) {
    let calls: Vec<(Entity, String, String, Value, Entity, Option<EffectId>)> = {
        let mut query = world.query_filtered::<
            (Entity, &ToolRequest, &ForAgent, Option<&Effect>),
            (ReadyToStart, ToolCalls),
        >();
        query
            .iter(world)
            .filter(|(call, _, _, _)| world.get::<Approved>(*call).is_none())
            .filter(|(call, _, _, _)| {
                world
                    .get::<Decision>(*call)
                    .is_none_or(|decision| decision.get().is_none())
            })
            .map(|(call, request, agent, effect)| {
                (
                    call,
                    request.call.id.wire().into_owned(),
                    request.call.function.name.as_str().to_owned(),
                    Value::Object(request.call.function.arguments.clone()),
                    agent.0,
                    effect.and_then(|effect| effect.parent),
                )
            })
            .collect()
    };
    if calls.is_empty() {
        return;
    }
    let mut reply_calls: HashMap<EffectId, usize> = HashMap::new();
    {
        let mut spawned = world.query::<(&Effect, &ToolRequest)>();
        for (effect, _) in spawned.iter(world) {
            if let Some(parent) = effect.parent {
                *reply_calls.entry(parent).or_default() += 1;
            }
        }
    }
    let store = world.resource::<Store>().clone();
    let mcp_tools = crate::host::mcp::tool_servers(world);
    let plugin_tools = crate::host::providers::extensions::tool_owners(world);
    let mut cached_rules: Option<Vec<PolicyRule>> = None;
    for (call, call_id, tool_name, arguments, agent, parent) in calls {
        let asked = parent
            .and_then(|parent| reply_calls.get(&parent).copied())
            .unwrap_or(0);
        if !within_tool_limits(&call_id, &tool_name, asked) {
            refuse(
                world,
                call,
                "a reply exceeded the tool-call count or identifier length limit",
            );
            continue;
        }
        if is_host_tool(&tool_name) {
            continue;
        }
        let rules = if let Some(rules) = &cached_rules {
            rules.clone()
        } else {
            match store.policy_rules() {
                Ok(rules) => {
                    cached_rules = Some(rules.clone());
                    rules
                }
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "policy rules could not be read; failing closed",
                    );
                    continue;
                }
            }
        };
        let (companion_id, work_id, run_id) = binding_of(world, agent);
        if let Some(providers) = world
            .get_resource::<crate::host::providers::Providers>()
            .cloned()
            && !providers
                .with_extension(ene_plugin::Extension::Hook)
                .is_empty()
            && let Some(runtime) = world
                .get_resource::<crate::host::resources::HostRuntime>()
                .map(|r| r.0.clone())
        {
            let hook_store = store.clone();
            let hook_event = ene_plugin::HookEvent::PreTool {
                companion_id: companion_id.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
            };
            let outcome = tokio::task::block_in_place(|| {
                runtime.block_on(crate::host::providers::extensions::run_hooks(
                    &providers,
                    &hook_store,
                    hook_event,
                ))
            });
            if let Some(reason) = outcome.blocked {
                refuse(world, call, &reason);
                continue;
            }
        }
        let (action, bound_client_id, bound_capability) = if let Some(server_id) =
            mcp_tools.get(&tool_name)
        {
            (
                CandidateAction::McpTool {
                    server_id: server_id.clone(),
                    tool_name: tool_name.clone(),
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else if let Some(plugin_id) = plugin_tools.get(&tool_name) {
            (
                CandidateAction::PluginTool {
                    plugin_id: plugin_id.clone(),
                    tool_name: tool_name.clone(),
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else if tool_name == crate::host::web_search::NAME {
            let providers = world
                .get_resource::<crate::host::providers::Providers>()
                .cloned()
                .unwrap_or_default();
            let resolved = match crate::host::web_search::resolve(
                &store,
                world.get_resource::<crate::host::model::Credentials>(),
                &providers,
            ) {
                Ok(resolved) => resolved,
                Err(reason) => {
                    refuse(world, call, &reason);
                    continue;
                }
            };
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (
                CandidateAction::WebSearch {
                    endpoint: resolved.endpoint,
                    query,
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else {
            let client_id = match store.target(&companion_id, work_id.as_deref()) {
                Ok(Some(client_id)) => client_id,
                Ok(None) => {
                    refuse(
                        world,
                        call,
                        "no_active_client: no client is bound to this agent",
                    );
                    continue;
                }
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "client binding could not be read; failing closed",
                    );
                    continue;
                }
            };
            let client = match store.client(&client_id) {
                Ok(Some(client)) => client,
                Ok(None) => {
                    refuse(world, call, "the bound client is not registered");
                    continue;
                }
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "client record could not be read; failing closed",
                    );
                    continue;
                }
            };
            let resolved = match resolve_capability(world, agent, &client, &tool_name, &arguments) {
                Ok(resolved) => resolved,
                Err(error) => {
                    refuse(world, call, error.message());
                    continue;
                }
            };
            let Some((capability, spec)) = resolved else {
                refuse(world, call, &format!("unknown device tool `{tool_name}`"));
                continue;
            };
            if !client.is_granted(&capability) {
                refuse(
                    world,
                    call,
                    &format!("capability `{capability}` is not granted"),
                );
                continue;
            }
            let empty = serde_json::json!({});
            let input = if tool_name == "device_request" {
                arguments.get("input").unwrap_or(&empty).clone()
            } else {
                arguments.clone()
            };
            if (capability.starts_with(crate::host::client::WORKSPACE_CAPABILITY_PREFIX)
                || capability == w::COMPUTER_USE)
                && let Some(spec) = &spec
                && let Err(error) = crate::host::client::validate_input(spec, &input)
            {
                refuse(world, call, error.message());
                continue;
            }
            (
                CandidateAction::DeviceCapability {
                    capability: capability.clone(),
                    spec: spec.map(Box::new),
                    tool_name: tool_name.clone(),
                    input,
                    raw_arguments: arguments.clone(),
                },
                Some(client_id),
                Some(capability),
            )
        };
        let verdict = decide_action(&rules, &action);
        if let (Some(client_id), Some(capability)) =
            (bound_client_id.clone(), bound_capability.clone())
        {
            world.entity_mut(call).insert(CallBinding {
                client_id,
                capability: capability.clone(),
                computer_run: (capability == w::COMPUTER_USE).then(|| run_id.clone()),
            });
        }
        match verdict {
            Verdict::Allow => {}
            Verdict::Deny { reason } => refuse(world, call, &reason),
            Verdict::HumanRequired { target } => {
                let request_id = uuid::Uuid::new_v4().to_string();
                let (normalized, hash) = canonicalise(&tool_name, &arguments);
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    work_id: work_id.clone(),
                    action_payload_hash: hash,
                    tool_name: tool_name.clone(),
                    normalized_arguments: normalized,
                    status: ApprovalStatus::Pending,
                    created_at: crate::host::util::now(),
                    resolved_at: None,
                    client_id: bound_client_id,
                    capability: bound_capability,
                };
                if let Err(error) = issue_hold(world, &store, approval, call) {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return;
                    }
                    refuse(world, call, "approval persistence failed; failing closed");
                    return;
                }
                let pending = Pending {
                    request: request_id,
                    companion: companion_id.clone(),
                    target,
                };
                push_card(world, &pending);
            }
            Verdict::NeedsDecision { target } => {
                let request_id = uuid::Uuid::new_v4().to_string();
                let (normalized, hash) = canonicalise(&tool_name, &arguments);
                let arguments_view = truncate(&normalized, DECISION_ARGUMENT_CHARS);
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    work_id: work_id.clone(),
                    action_payload_hash: hash,
                    tool_name: tool_name.clone(),
                    normalized_arguments: normalized,
                    status: ApprovalStatus::Pending,
                    created_at: crate::host::util::now(),
                    resolved_at: None,
                    client_id: bound_client_id,
                    capability: bound_capability,
                };
                if let Err(error) = issue_hold(world, &store, approval, call) {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return;
                    }
                    refuse(world, call, "approval persistence failed; failing closed");
                    return;
                }
                let pending = Pending {
                    request: request_id,
                    companion: companion_id.clone(),
                    target: target.clone(),
                };
                let policies = match store.policy_texts() {
                    Ok(policies) => policies,
                    Err(error) => {
                        if error.is_storage() {
                            crate::host::fail(world, error.to_string());
                            return;
                        }
                        push_card(world, &pending);
                        continue;
                    }
                };
                let inputs = DecisionInputs {
                    tool_name: tool_name.clone(),
                    target,
                    arguments: arguments_view,
                    request: request_context(world, &store, agent, &run_id),
                    policies,
                    rules: rules.clone(),
                };
                start_decision(world, &store, pending, inputs);
            }
        }
    }
}

pub(crate) fn issue_hold(
    world: &mut World,
    store: &Store,
    approval: Approval,
    call: Entity,
) -> Result<Hold> {
    let request_id = approval.request_id.clone();
    store.insert_approval(&approval)?;
    let hold = world.spawn(HoldOf(call)).id();
    let entry = Hold { call, hold };
    world
        .resource_mut::<Approvals>()
        .by_request
        .insert(request_id, entry);
    Ok(entry)
}

pub(crate) fn notify_approval_card(
    world: &mut World,
    companion: &str,
    approval: &Approval,
    target: &str,
) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
            request: Some(crate::host::wire::approval(approval)).into(),
            ..Default::default()
        })),
    );
    notifier.notice(
        Some(companion),
        "approval_required",
        &format!("{target}: approval is required before this operation runs"),
    );
}

/// The companion, work session and run an agent serves. A work agent carries
/// both ids; a companion agent leaves the work ones empty.
fn binding_of(world: &World, agent: Entity) -> (String, Option<String>, String) {
    if let Some(companion) = world.get::<CompanionAgent>(agent) {
        return (companion.companion_id.clone(), None, String::new());
    }
    match world.get::<WorkAgent>(agent) {
        Some(work) => (
            work.companion_id.clone(),
            Some(work.work_id.clone()),
            work.run_id.clone(),
        ),
        None => (String::new(), None, String::new()),
    }
}

/// The capability a device tool call names, with its declaration.
fn resolve_capability(
    world: &mut World,
    agent: Entity,
    client: &Client,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<(String, Option<CapabilitySpec>)>> {
    if tool_name == "device_request" {
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
        if capability.starts_with(crate::host::client::WORKSPACE_CAPABILITY_PREFIX)
            && world.get::<WorkAgent>(agent).is_none()
        {
            return Err(HostError::Permission(
                ("workspace capabilities require a work session").into(),
            ));
        }
        if capability == w::COMPUTER_USE
            && (world.get::<WorkAgent>(agent).is_none()
                || !crate::host::client::computer::model_supported(
                    world.get::<rig_ecs::model::Connection>(agent),
                ))
        {
            return Err(HostError::Permission(
                ("Computer Use requires an internal work agent with an image and tool capable task model").into(),
            ));
        }
        return Ok(Some((
            capability.to_owned(),
            client.spec(capability).cloned(),
        )));
    }
    if world.get::<WorkAgent>(agent).is_some() {
        return Err(HostError::Permission(
            ("capability tools are offered to the parent only").into(),
        ));
    }
    let Some(spec) = crate::host::client::dynamic::tool_spec_for(world, client, tool_name)? else {
        return Ok(None);
    };
    Ok(Some((spec.name.clone(), Some(spec))))
}

#[cfg(test)]
mod tool_limit_tests {
    use super::*;

    #[test]
    fn the_old_reply_count_and_identifier_lengths_are_preserved() {
        assert!(within_tool_limits("id", "lookup", 64));
        assert!(!within_tool_limits("id", "lookup", 65));
        assert!(within_tool_limits(&"i".repeat(256), &"n".repeat(256), 64));
        assert!(!within_tool_limits(&"i".repeat(257), "lookup", 1));
        assert!(!within_tool_limits("id", &"n".repeat(257), 1));
    }
}
