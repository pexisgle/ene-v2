//! The permission gate (`Stage::Gate`): limits, hooks, rule evaluation, and holds.
use super::approval::{expire_orphaned_approvals, refuse};
use super::conversation::{Replies, collect_replies, latest_user_request, register_approval_tool};
use super::decision::{
    DECISION_ARGUMENT_CHARS, DecisionInputs, Decisions, Pending, collect_decisions, push_card,
    request_context, start_decision,
};
use super::resolve::{
    AgentCapabilityContext, CandidateOutcome, PreToolChecked, PreparedPluginTool, binding_of,
    build_candidate_action, collect_pre_tool_hooks, count_reply_calls, load_gate_rules,
    resolve_device_candidate, start_pre_tool_hooks,
};
use crate::host::{
    db::Store,
    error::Result,
    notify::Notifier,
    resources::{HostExternalTool, WorkAgent},
};
use bevy_ecs::prelude::*;
use ene_policy::{
    Approval, ApprovalStatus, PolicyRule, PolicyText, Verdict, canonicalise, decide_action,
    is_host_tool,
};
use ene_rpc as w;
use ene_store::util::truncate;
use rig_core::effect::EffectId;
use rig_ecs::prelude::*;
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

/// The marker the host inserts on a call whose approval was granted.
#[derive(Clone, Component)]
pub(crate) enum Approved {
    Human,
    Decision {
        texts: Vec<PolicyText>,
        rules: Vec<PolicyRule>,
    },
}

/// What the call was accepted against. The device executor dispatches to it.
#[derive(Component, Clone, Debug)]
pub(crate) struct CallBinding {
    pub(crate) client_id: String,
    pub(crate) capability: String,
    pub(crate) work_run: Option<String>,
}

/// A held tool call and the marker that keeps it from running.
#[derive(Clone)]
pub(crate) struct Hold {
    pub(crate) call: Entity,
    pub(crate) hold: Entity,
    pub(super) agent: Option<Entity>,
    pub(super) after_input: Option<RequestId>,
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
            .init_resource::<Replies>()
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(Update, collect_pre_tool_hooks.before(AgentLoop))
            .add_systems(
                Update,
                (
                    expire_orphaned_approvals,
                    collect_decisions,
                    collect_replies,
                )
                    .chain()
                    .after(AgentLoop),
            );
        register_approval_tool(app);
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
    pub(super) external_tools: HashMap<String, HostExternalTool>,
    pub(super) providers: Option<crate::host::providers::Providers>,
    pub(super) credentials: Option<crate::host::model::Credentials>,
    pub(super) runtime: Option<tokio::runtime::Handle>,
    pub(super) wake: Option<Wake>,
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
        external_tools: world
            .query::<(&ToolDef, &HostExternalTool)>()
            .iter(world)
            .map(|(definition, owner)| (definition.0.name.as_str().to_owned(), owner.clone()))
            .collect(),
        providers: world
            .get_resource::<crate::host::providers::Providers>()
            .cloned(),
        credentials: world
            .get_resource::<crate::host::model::Credentials>()
            .cloned(),
        runtime: world
            .get_resource::<crate::host::resources::HostRuntime>()
            .map(|r| r.0.clone()),
        wake: world.get_resource::<Wake>().cloned(),
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
        .filter(|(call, _, _, _)| {
            world.get::<Approved>(*call).is_none()
                || world
                    .get::<PreparedPluginTool>(*call)
                    .is_some_and(PreparedPluginTool::revoked)
        })
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
    if world
        .get::<PreparedPluginTool>(item.entity)
        .is_some_and(PreparedPluginTool::revoked)
    {
        refuse(
            world,
            item.entity,
            "the tool plugin was replaced or disabled",
        );
        return std::ops::ControlFlow::Continue(());
    }
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
                crate::host::fail(world, &error.to_string());
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
    let Some((companion_id, work_id, run_id)) =
        binding_of(world, &ctx.store, item.agent, item.entity)
    else {
        return std::ops::ControlFlow::Continue(());
    };
    tracing::Span::current().record("companion_id", companion_id.as_str());
    if world.get::<PreToolChecked>(item.entity).is_none() {
        match start_pre_tool_hooks(ctx, &item, &companion_id) {
            Ok(Some(hook)) => {
                world.spawn((HoldOf(item.entity), hook));
                return std::ops::ControlFlow::Continue(());
            }
            Err(error) => {
                refuse(world, item.entity, &error.to_string());
                return std::ops::ControlFlow::Continue(());
            }
            Ok(None) => {}
        }
    }
    let candidate = match build_candidate_action(ctx, &item) {
        CandidateOutcome::Resolved(candidate) => Some(*candidate),
        CandidateOutcome::Search(search) => {
            let action = search.candidate();
            world.entity_mut(item.entity).insert(*search);
            Some((action, None, None))
        }
        CandidateOutcome::Plugin(tool) => {
            let action = tool.candidate();
            world.entity_mut(item.entity).insert(tool);
            Some((action, None, None))
        }
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
                &agent_ctx,
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
            work_run: work_id.as_ref().map(|_| run_id.clone()),
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
    apply_gate_verdict(world, &ctx.store, &rules, &binding, verdict)
}

fn apply_gate_verdict(
    world: &mut World,
    store: &Store,
    rules: &[PolicyRule],
    binding: &GateBinding,
    verdict: Verdict,
) -> std::ops::ControlFlow<()> {
    match verdict {
        Verdict::Allow => {}
        Verdict::Deny { reason } => refuse(world, binding.call.entity, &reason),
        Verdict::HumanRequired { target } => {
            let (approval, target) =
                issue_gate_hold(world, store, binding, target, ApprovalStatus::Pending)?;
            notify_approval_card(
                &mut world.resource_mut::<Notifier>(),
                &approval.session_id,
                &approval,
                &target,
            );
        }
        Verdict::NeedsDecision { target } => {
            let (approval, target) =
                issue_gate_hold(world, store, binding, target, ApprovalStatus::Evaluating)?;
            let arguments = truncate(&approval.review_arguments(), DECISION_ARGUMENT_CHARS);
            let pending = Pending {
                request: approval.request_id,
                companion: approval.session_id,
                target,
            };
            let policies = match store.policy_repo().policy_texts() {
                Ok(policies) => policies,
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, &error.to_string());
                        return std::ops::ControlFlow::Break(());
                    }
                    if let Some(fatal) =
                        push_card(store, &mut world.resource_mut::<Notifier>(), &pending)
                    {
                        crate::host::fail(world, &fatal);
                        return std::ops::ControlFlow::Break(());
                    }
                    return std::ops::ControlFlow::Continue(());
                }
            };
            let inputs = DecisionInputs {
                tool_name: approval.tool_name,
                target: pending.target.clone(),
                arguments,
                request: request_context(
                    store,
                    world.get::<Conversation>(binding.call.agent),
                    &approval.run_id,
                ),
                policies,
                rules: rules.to_vec(),
            };
            start_decision(world, store, &pending, inputs);
        }
    }
    std::ops::ControlFlow::Continue(())
}

fn issue_gate_hold(
    world: &mut World,
    store: &Store,
    binding: &GateBinding,
    target: String,
    status: ApprovalStatus,
) -> std::ops::ControlFlow<(), (Approval, String)> {
    let (normalized, hash) = canonicalise(&binding.call.tool_name, &binding.call.arguments);
    let search = world.get::<crate::host::web_search::PreparedSearch>(binding.call.entity);
    let external_destination =
        search.map(|search| search.route.binding.display_destination.clone());
    let target = external_destination.clone().unwrap_or(target);
    let approval = Approval {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: binding.companion_id.clone(),
        run_id: binding.run_id.clone(),
        work_id: binding.work_id.clone(),
        action_payload_hash: hash,
        tool_name: binding.call.tool_name.clone(),
        normalized_arguments: normalized,
        status,
        created_at: ene_store::util::now(),
        resolved_at: None,
        client_id: binding.bound_client_id.clone(),
        capability: binding.bound_capability.clone(),
        external_destination,
        search_summary: search.map(crate::host::web_search::PreparedSearch::summary),
    };
    if let Err(error) = issue_hold(world, store, &approval, binding.call.entity) {
        if error.is_storage() {
            crate::host::fail(world, &error.to_string());
            return std::ops::ControlFlow::Break(());
        }
        refuse(
            world,
            binding.call.entity,
            "approval persistence failed; failing closed",
        );
        return std::ops::ControlFlow::Break(());
    }
    std::ops::ControlFlow::Continue((approval, target))
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
    approval: &Approval,
    call: Entity,
) -> Result<Hold> {
    let request_id = approval.request_id.clone();
    store.approval_repo().insert_approval(approval)?;
    let hold = world.spawn(HoldOf(call)).id();
    let after_input = crate::host::agent::agent_of(world, &approval.session_id)
        .and_then(|agent| world.get::<Conversation>(agent))
        .and_then(latest_user_request)
        .cloned();
    let entry = Hold {
        call,
        hold,
        agent: world.get::<ForAgent>(call).map(|owner| owner.0),
        after_input,
    };
    world
        .resource_mut::<Approvals>()
        .by_request
        .insert(request_id, entry.clone());
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
