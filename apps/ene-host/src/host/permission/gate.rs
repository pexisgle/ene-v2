//! The permission gate (`Stage::Gate`): limits, hooks, rule evaluation, and holds.
use super::approval::{expire_orphaned_approvals, refuse, resolve_approval};
use super::decision::{
    DECISION_ARGUMENT_CHARS, DecisionInputs, Decisions, Pending, collect_decisions, push_card,
    request_context, start_decision,
};
use super::resolve::{
    AgentCapabilityContext, CandidateOutcome, build_candidate_action, load_gate_rules,
    resolve_device_candidate, run_pre_tool_hooks,
};
use crate::host::{
    db::Store,
    error::Result,
    notify::Notifier,
    resources::{CompanionAgent, WorkAgent},
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use ene_host_core::util::truncate;
use ene_policy::{
    Approval, ApprovalStatus, PolicyRule, Verdict, canonicalise, decide_action, is_host_tool,
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
pub(super) struct GateCall {
    pub(super) entity: Entity,
    call_id: String,
    pub(super) tool_name: String,
    pub(super) arguments: Value,
    agent: Entity,
    parent: Option<EffectId>,
}

pub(super) struct GateContext {
    pub(super) store: Store,
    pub(super) mcp_tools: HashMap<String, String>,
    pub(super) plugin_tools: HashMap<String, String>,
    pub(super) providers: Option<crate::host::providers::Providers>,
    pub(super) credentials: Option<crate::host::model::Credentials>,
    pub(super) runtime: Option<tokio::runtime::Handle>,
    pub(super) cached_rules: Option<Vec<PolicyRule>>,
    reply_calls: HashMap<EffectId, usize>,
}

struct GateBinding {
    call: GateCall,
    companion_id: String,
    work_id: Option<String>,
    run_id: String,
    bound_client_id: Option<String>,
    bound_capability: Option<String>,
}

fn gate(world: &mut World) {
    let calls = collect_pending_gate_calls(world);
    if calls.is_empty() {
        return;
    }
    let mut ctx = GateContext {
        store: world.resource::<Store>().clone(),
        mcp_tools: crate::host::mcp::tool_servers(world),
        plugin_tools: crate::host::providers::extensions::tool_owners(world),
        providers: world
            .get_resource::<crate::host::providers::Providers>()
            .cloned(),
        credentials: world
            .get_resource::<crate::host::model::Credentials>()
            .cloned(),
        runtime: world
            .get_resource::<crate::host::resources::HostRuntime>()
            .map(|r| r.0.clone()),
        cached_rules: None,
        reply_calls: count_reply_calls(world),
    };
    for item in calls {
        if process_gate_call(world, &mut ctx, item).is_break() {
            return;
        }
    }
}

fn collect_pending_gate_calls(world: &mut World) -> Vec<GateCall> {
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
        .map(|(entity, request, agent, effect)| GateCall {
            entity,
            call_id: request.call.id.wire().into_owned(),
            tool_name: request.call.function.name.as_str().to_owned(),
            arguments: Value::Object(request.call.function.arguments.clone()),
            agent: agent.0,
            parent: effect.and_then(|effect| effect.parent),
        })
        .collect()
}

fn count_reply_calls(world: &mut World) -> HashMap<EffectId, usize> {
    let mut reply_calls = HashMap::new();
    let mut spawned = world.query::<(&Effect, &ToolRequest)>();
    for (effect, _) in spawned.iter(world) {
        if let Some(parent) = effect.parent {
            *reply_calls.entry(parent).or_default() += 1;
        }
    }
    reply_calls
}

#[tracing::instrument(
    name = "permission.gate",
    skip_all,
    fields(
        tool = %item.tool_name,
        call_id = %item.call_id,
        companion_id = tracing::field::Empty,
    )
)]
fn process_gate_call(
    world: &mut World,
    ctx: &mut GateContext,
    item: GateCall,
) -> std::ops::ControlFlow<()> {
    let asked = item
        .parent
        .and_then(|parent| ctx.reply_calls.get(&parent).copied())
        .unwrap_or(0);
    if !within_tool_limits(&item.call_id, &item.tool_name, asked) {
        refuse(
            world,
            item.entity,
            "a reply exceeded the tool-call count or identifier length limit",
        );
        return std::ops::ControlFlow::Continue(());
    }
    if is_host_tool(&item.tool_name) {
        return std::ops::ControlFlow::Continue(());
    }
    let rules = match load_gate_rules(ctx) {
        Ok(rules) => rules,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            } else {
                refuse(
                    world,
                    item.entity,
                    "policy rules could not be read; failing closed",
                );
            }
            return std::ops::ControlFlow::Break(());
        }
    };
    let (companion_id, work_id, run_id) = binding_of(world, item.agent);
    tracing::Span::current().record("companion_id", companion_id.as_str());
    if let Some(reason) = run_pre_tool_hooks(ctx, &companion_id, &item.tool_name, &item.arguments) {
        refuse(world, item.entity, &reason);
        return std::ops::ControlFlow::Continue(());
    }
    let candidate = match build_candidate_action(ctx, &item) {
        CandidateOutcome::Resolved(candidate) => Some(*candidate),
        CandidateOutcome::NeedsDeviceClient => {
            let agent_ctx = AgentCapabilityContext {
                is_work_agent: world.get::<WorkAgent>(item.agent).is_some(),
                computer_model_supported: crate::host::client::computer::model_supported(
                    world.get::<rig_ecs::model::Connection>(item.agent),
                ),
            };
            resolve_device_candidate(
                world,
                &ctx.store,
                &companion_id,
                work_id.as_deref(),
                agent_ctx,
                &item,
            )
        }
        CandidateOutcome::Refuse(reason) => {
            refuse(world, item.entity, &reason);
            None
        }
    };
    let Some((action, bound_client_id, bound_capability)) = candidate else {
        return std::ops::ControlFlow::Continue(());
    };
    let verdict = decide_action(&rules, &action);
    if let (Some(client_id), Some(capability)) = (bound_client_id.clone(), bound_capability.clone())
    {
        world.entity_mut(item.entity).insert(CallBinding {
            client_id,
            capability: capability.clone(),
            computer_run: (capability == w::COMPUTER_USE).then(|| run_id.clone()),
        });
    }
    let binding = GateBinding {
        call: item,
        companion_id,
        work_id,
        run_id,
        bound_client_id,
        bound_capability,
    };
    apply_gate_verdict(world, &ctx.store, &rules, binding, verdict)
}

fn apply_gate_verdict(
    world: &mut World,
    store: &Store,
    rules: &[PolicyRule],
    binding: GateBinding,
    verdict: Verdict,
) -> std::ops::ControlFlow<()> {
    match verdict {
        Verdict::Allow => {}
        Verdict::Deny { reason } => refuse(world, binding.call.entity, &reason),
        Verdict::HumanRequired { target } => {
            let (pending, _) = issue_gate_hold(world, store, &binding, target)?;
            if let Some(fatal) = push_card(store, &mut world.resource_mut::<Notifier>(), &pending) {
                crate::host::fail(world, fatal);
                return std::ops::ControlFlow::Break(());
            }
        }
        Verdict::NeedsDecision { target } => {
            let agent = binding.call.agent;
            let tool_name = binding.call.tool_name.clone();
            let run_id = binding.run_id.clone();
            let (pending, arguments_view) = issue_gate_hold(world, store, &binding, target)?;
            let policies = match store.policy_repo().policy_texts() {
                Ok(policies) => policies,
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                        return std::ops::ControlFlow::Break(());
                    }
                    if let Some(fatal) =
                        push_card(store, &mut world.resource_mut::<Notifier>(), &pending)
                    {
                        crate::host::fail(world, fatal);
                        return std::ops::ControlFlow::Break(());
                    }
                    return std::ops::ControlFlow::Continue(());
                }
            };
            let inputs = DecisionInputs {
                tool_name,
                target: pending.target.clone(),
                arguments: arguments_view,
                request: request_context(store, world.get::<Conversation>(agent), &run_id),
                policies,
                rules: rules.to_vec(),
            };
            start_decision(world, store, pending, inputs);
        }
    }
    std::ops::ControlFlow::Continue(())
}

fn issue_gate_hold(
    world: &mut World,
    store: &Store,
    binding: &GateBinding,
    target: String,
) -> std::ops::ControlFlow<(), (Pending, String)> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let (normalized, hash) = canonicalise(&binding.call.tool_name, &binding.call.arguments);
    let arguments_view = truncate(&normalized, DECISION_ARGUMENT_CHARS);
    let approval = Approval {
        request_id: request_id.clone(),
        session_id: binding.companion_id.clone(),
        run_id: binding.run_id.clone(),
        work_id: binding.work_id.clone(),
        action_payload_hash: hash,
        tool_name: binding.call.tool_name.clone(),
        normalized_arguments: normalized,
        status: ApprovalStatus::Pending,
        created_at: ene_host_core::util::now(),
        resolved_at: None,
        client_id: binding.bound_client_id.clone(),
        capability: binding.bound_capability.clone(),
    };
    if let Err(error) = issue_hold(world, store, approval, binding.call.entity) {
        if error.is_storage() {
            crate::host::fail(world, error.to_string());
            return std::ops::ControlFlow::Break(());
        }
        refuse(
            world,
            binding.call.entity,
            "approval persistence failed; failing closed",
        );
        return std::ops::ControlFlow::Break(());
    }
    let pending = Pending {
        request: request_id,
        companion: binding.companion_id.clone(),
        target,
    };
    std::ops::ControlFlow::Continue((pending, arguments_view))
}

#[tracing::instrument(
    name = "permission.issue_hold",
    skip_all,
    fields(
        request_id = %approval.request_id,
        companion_id = %approval.session_id,
        tool = %approval.tool_name,
    )
)]
pub(crate) fn issue_hold(
    world: &mut World,
    store: &Store,
    approval: Approval,
    call: Entity,
) -> Result<Hold> {
    let request_id = approval.request_id.clone();
    store.approval_repo().insert_approval(&approval)?;
    let hold = world.spawn(HoldOf(call)).id();
    let entry = Hold { call, hold };
    world
        .resource_mut::<Approvals>()
        .by_request
        .insert(request_id, entry);
    Ok(entry)
}

pub(crate) fn notify_approval_card(
    notifier: &mut Notifier,
    companion: &str,
    approval: &Approval,
    target: &str,
) {
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
