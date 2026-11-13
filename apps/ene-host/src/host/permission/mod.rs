//! Approval requests, saved policy rules and the decision the Gate applies.
//!
//! The pipeline follows docs/design/permissions.md §3: hard denial, explicit
//! every-time confirmation, saved rules, then the user. The alpha does not run
//! the auxiliary decision model yet, so an unregistered device operation falls
//! through to the confirmation card (fail-safe).
use super::client::{CapabilityApproval, CapabilitySpec};
use super::db::{digest, truncate};
use super::error::Failure;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PolicyEffect {
    Allow,
    Deny,
    Confirm,
}

impl PolicyEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Confirm => "confirm",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            "confirm" => Self::Confirm,
            _ => return None,
        })
    }
}

/// A saved rule ("always approve" is one-shot in the alpha and saves no rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyRule {
    pub(crate) id: String,
    pub(crate) tool_name: String,
    /// Exact normalised arguments text, or an exact string argument value.
    pub(crate) pattern: String,
    pub(crate) effect: PolicyEffect,
    pub(crate) created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "denied" => Self::Denied,
            "expired" => Self::Expired,
            _ => return None,
        })
    }
}

/// An approval request, immutable in the parts that identify the operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Approval {
    pub(crate) request_id: String,
    /// The agent the call belongs to (companion for the head, work session's
    /// run for the hand).
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) action_payload_hash: Vec<u8>,
    pub(crate) tool_name: String,
    pub(crate) normalized_arguments: String,
    pub(crate) status: ApprovalStatus,
    pub(crate) created_at: String,
    pub(crate) resolved_at: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) activation_revision: Option<u64>,
    pub(crate) capability: Option<String>,
}

/// What the Gate should do with a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    Confirm { target: String },
    Deny { reason: String },
}

/// Capability names the host never offers, whatever a client declares.
const HARD_DENIED: &[&str] = &["payment.", "securities.", "banking.", "crypto.transfer."];

/// Tools that belong to the host process itself. The permission docs never
/// gate them: they are conversation, memory and work management.
pub(crate) fn is_host_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "mind_recall"
            | "mind_read"
            | "mind_update"
            | "start_work"
            | "send_work"
            | "list_works"
            | "stop_work"
            | "noop"
            | "report"
            | "schedule_create"
            | "schedule_list"
            | "schedule_cancel"
    )
}

pub(crate) fn hard_denied(capability: &str) -> Option<&'static str> {
    HARD_DENIED
        .iter()
        .find(|prefix| capability.starts_with(**prefix))
        .copied()
}

/// Canonical tool arguments and their SHA-256. Key order is canonical because
/// `serde_json::Value` objects are ordered maps.
pub(crate) fn canonicalise(tool_name: &str, arguments: &Value) -> (String, Vec<u8>) {
    let normalized = arguments.to_string();
    let hash = digest(format!("{tool_name}\n{normalized}").as_bytes());
    (normalized, hash)
}

/// The decision for a capability operation before the card is shown.
///
/// `spec` is the declared capability (absent for the generic dispatcher when
/// the capability was never declared). `rules` are the saved policy rules.
pub(crate) fn decide_capability(
    rules: &[PolicyRule],
    spec: Option<&CapabilitySpec>,
    capability: &str,
    tool_name: &str,
    arguments: &Value,
) -> Verdict {
    if let Some(prefix) = hard_denied(capability) {
        return Verdict::Deny {
            reason: format!("`{capability}` is hard-blocked by the host ({prefix}*)"),
        };
    }
    let (normalized, _) = canonicalise(tool_name, arguments);
    let mut denied: Option<&PolicyRule> = None;
    let mut confirm = false;
    let mut allowed = false;
    for rule in rules
        .iter()
        .filter(|rule| matches(rule, tool_name, &normalized, arguments))
    {
        match rule.effect {
            PolicyEffect::Deny => {
                denied = Some(rule);
                break;
            }
            PolicyEffect::Confirm => confirm = true,
            PolicyEffect::Allow => allowed = true,
        }
    }
    // A saved deny always wins. An explicit every-time-confirm rule outranks a
    // saved allow, and an allow bypasses only the capability's default need
    // for confirmation (not a hard block or a confirm rule).
    if let Some(rule) = denied {
        return Verdict::Deny {
            reason: format!("saved rule `{}` denies this operation", rule.id),
        };
    }
    if confirm {
        return Verdict::Confirm {
            target: target(spec, capability, arguments),
        };
    }
    if allowed {
        return Verdict::Allow;
    }
    match spec {
        Some(spec) if spec.approval == CapabilityApproval::Auto => Verdict::Allow,
        // A declared capability that expects confirmation, an undeclared one
        // and an unknown approval all fail safe to the card.
        _ => Verdict::Confirm {
            target: target(spec, capability, arguments),
        },
    }
}

fn matches(rule: &PolicyRule, tool_name: &str, normalized: &str, arguments: &Value) -> bool {
    if rule.tool_name != "*" && rule.tool_name != tool_name {
        return false;
    }
    rule.pattern == "*"
        || rule.pattern == normalized
        || arguments
            .as_object()
            .into_iter()
            .flat_map(|object| object.values())
            .filter_map(Value::as_str)
            .any(|value| value == rule.pattern)
}

fn target(spec: Option<&CapabilitySpec>, capability: &str, arguments: &Value) -> String {
    match spec {
        Some(spec) => super::client::approval_target(spec, arguments),
        None => format!("{capability} {}", truncate(&arguments.to_string(), 200)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalDecision {
    Unspecified,
    Approve,
    /// One-shot in this alpha; the caller may add a rule explicitly later.
    AlwaysApprove,
    Deny,
}

impl From<Failure> for Verdict {
    fn from(failure: Failure) -> Self {
        Self::Deny {
            reason: failure.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(effect: PolicyEffect, tool: &str, pattern: &str) -> PolicyRule {
        PolicyRule {
            id: "r1".into(),
            tool_name: tool.into(),
            pattern: pattern.into(),
            effect,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn spec(approval: CapabilityApproval) -> CapabilitySpec {
        CapabilitySpec {
            name: "device.files".into(),
            tool_name: String::new(),
            description: "files".into(),
            input_schema: r#"{"type":"object"}"#.into(),
            presets: Vec::new(),
            approval,
            approval_target: String::new(),
            output_schema: String::new(),
        }
    }

    #[test]
    fn hard_denied_capabilities_never_reach_a_card() {
        let verdict = decide_capability(
            &[],
            Some(&spec(CapabilityApproval::Auto)),
            "payment.transfer",
            "device_request",
            &json!({}),
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
    }

    #[test]
    fn confirm_and_deny_rules_outrank_allows() {
        let allow = rule(PolicyEffect::Allow, "device_request", "*");
        let confirm = rule(PolicyEffect::Confirm, "device_request", "/tmp/x");
        let denied = decide_capability(
            &[allow.clone(), confirm],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert!(matches!(denied, Verdict::Confirm { .. }));
        let stamped = {
            let mut confirm = rule(PolicyEffect::Confirm, "*", "*");
            confirm.id = "r2".into();
            confirm
        };
        let allow_after = decide_capability(
            &[stamped],
            Some(&spec(CapabilityApproval::Auto)),
            "body.motion",
            "body_motion",
            &json!({"motion": "nod"}),
        );
        assert!(matches!(allow_after, Verdict::Confirm { .. }));
        let allow_only = decide_capability(
            &[allow],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/y"}),
        );
        assert_eq!(allow_only, Verdict::Allow);
    }

    #[test]
    fn undeclared_operations_fall_through_to_confirmation() {
        let verdict = decide_capability(&[], None, "device.screen", "device_request", &json!({}));
        assert!(matches!(verdict, Verdict::Confirm { .. }));
        let auto = decide_capability(
            &[],
            Some(&spec(CapabilityApproval::Auto)),
            "body.motion",
            "body_motion",
            &json!({"motion": "nod"}),
        );
        assert_eq!(auto, Verdict::Allow);
    }

    #[test]
    fn a_deny_rule_beats_a_confirm_rule_and_an_allow_bypasses_a_default_confirm() {
        let confirm = rule(PolicyEffect::Confirm, "device_request", "*");
        let mut deny = rule(PolicyEffect::Deny, "device_request", "*");
        deny.id = "r2".into();
        let verdict = decide_capability(
            &[confirm, deny],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));

        let allow = rule(PolicyEffect::Allow, "device_request", "*");
        let verdict = decide_capability(
            &[allow],
            Some(&spec(CapabilityApproval::Confirm)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn arguments_are_canonical_and_hashes_are_stable() {
        let (first, hash) = canonicalise("device_request", &json!({"b": 1, "a": 2}));
        let (second, other) = canonicalise("device_request", &json!({"a": 2, "b": 1}));
        assert_eq!(first, second);
        assert_eq!(hash, other);
        let (_, changed) = canonicalise("device_request", &json!({"a": 2, "b": 2}));
        assert_ne!(hash, changed);
    }
}

// === Gate =================================================================

use super::client::Client;
use super::db::Store;
use super::notify::Notifier;
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// The marker the host inserts on a call whose approval was granted.
#[derive(Component)]
pub(crate) struct Approved;

/// What the call was accepted against. The device executor revalidates it.
#[derive(Component, Clone, Debug)]
pub(crate) struct CallBinding {
    pub(crate) client_id: String,
    pub(crate) activation_revision: u64,
    pub(crate) capability: String,
}

/// A held tool call and the request that released it.
pub(crate) struct Hold {
    pub(crate) call: Entity,
    pub(crate) hold: Entity,
    /// The approval the hold released, kept for generation checks.
    pub(crate) approval: Approval,
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
        app.add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(Update, expire_orphaned_approvals.after(AgentLoop));
    }
}

/// Applies the decision pipeline to every tool call that is about to start.
/// Host-owned tools pass; device capabilities are resolved to a client,
/// checked against the saved rules and, when needed, held for approval.
fn gate(world: &mut World) {
    let calls: Vec<(Entity, String, Value, Entity)> = {
        let mut query =
            world.query_filtered::<(Entity, &ToolRequest, &ForAgent), (ReadyToStart, ToolCalls)>();
        query
            .iter(world)
            .filter(|(call, _, _)| world.get::<Approved>(*call).is_none())
            .map(|(call, request, agent)| {
                (
                    call,
                    request.call.function.name.as_str().to_owned(),
                    Value::Object(request.call.function.arguments.clone()),
                    agent.0,
                )
            })
            .collect()
    };
    if calls.is_empty() {
        return;
    }
    let store = world.resource::<Store>().clone();
    let rules = store.policy_rules().unwrap_or_default();
    for (call, tool_name, arguments, agent) in calls {
        if is_host_tool(&tool_name) {
            continue;
        }
        let (companion_id, work_id, run_id, bound) = binding_of(world, agent);
        let Some((client_id, activation_revision)) =
            target_client(&companion_id, &store, &bound, &work_id)
        else {
            decide(
                world,
                call,
                Outcome::refused("no_active_client: no client is bound to this agent"),
            );
            continue;
        };
        let Some(client) = store.client(&client_id).ok().flatten() else {
            decide(
                world,
                call,
                Outcome::refused("the bound client is not registered"),
            );
            continue;
        };
        // A tool that is neither a host tool nor a declared capability is
        // refused: a future tool must opt into one of the two paths.
        let Some((capability, input, spec)) =
            resolve_capability(world, &client, &tool_name, &arguments)
        else {
            decide(
                world,
                call,
                Outcome::refused(format!("unknown device tool `{tool_name}`")),
            );
            continue;
        };
        if !client.is_granted(&capability) {
            decide(
                world,
                call,
                Outcome::refused(format!("capability `{capability}` is not granted")),
            );
            continue;
        }
        let verdict = decide_capability(&rules, spec.as_ref(), &capability, &tool_name, &input);
        world.entity_mut(call).insert(CallBinding {
            client_id: client_id.clone(),
            activation_revision,
            capability: capability.clone(),
        });
        match verdict {
            Verdict::Allow => {}
            Verdict::Deny { reason } => {
                decide(world, call, Outcome::refused(reason));
            }
            Verdict::Confirm { target } => {
                let hold = world.spawn(HoldOf(call)).id();
                let request_id = uuid::Uuid::new_v4().to_string();
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    action_payload_hash: canonicalise(&tool_name, &arguments).1,
                    tool_name: tool_name.clone(),
                    normalized_arguments: arguments.to_string(),
                    status: ApprovalStatus::Pending,
                    created_at: super::db::now(),
                    resolved_at: None,
                    client_id: Some(client_id.clone()),
                    activation_revision: Some(activation_revision),
                    capability: Some(capability.clone()),
                };
                if let Err(error) = store.insert_approval(&approval) {
                    super::fail(world, error.to_string());
                    return;
                }
                world.resource_mut::<Approvals>().by_request.insert(
                    request_id,
                    Hold {
                        call,
                        hold,
                        approval: approval.clone(),
                    },
                );
                let mut notifier = world.resource_mut::<Notifier>();
                notifier.push(
                    Some(&companion_id),
                    None,
                    w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
                        request: Some(crate::host::wire::approval(&approval)).into(),
                        ..Default::default()
                    })),
                );
                let mut notifier = world.resource_mut::<Notifier>();
                notifier.notice(
                    Some(&companion_id),
                    "approval_required",
                    &format!("{target}: approval is required before this operation runs"),
                );
            }
        }
    }
}

/// The companion, work session and run an agent serves.
fn binding_of(world: &World, agent: Entity) -> (String, String, String, AgentRole) {
    if let Some(companion) = world.get::<super::agent::CompanionAgent>(agent) {
        return (
            companion.companion_id.clone(),
            String::new(),
            String::new(),
            AgentRole::Parent,
        );
    }
    if let Some(work) = world.get::<super::agent::WorkAgent>(agent) {
        return (
            work.companion_id.clone(),
            work.work_id.clone(),
            work.run_id.clone(),
            AgentRole::Work,
        );
    }
    (
        String::new(),
        String::new(),
        String::new(),
        AgentRole::Parent,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentRole {
    Parent,
    Work,
}

/// The capability a device tool call names, with its input and declaration.
/// The client is already resolved, so the hand resolves against the client it
/// is bound to, not against the companion's current activation.
fn resolve_capability(
    world: &mut World,
    client: &Client,
    tool_name: &str,
    arguments: &Value,
) -> Option<(String, Value, Option<CapabilitySpec>)> {
    if tool_name == "device_request" {
        let capability = arguments.get("capability")?.as_str()?.to_owned();
        if !client.capabilities.iter().any(|name| name == &capability) {
            return None;
        }
        let input = arguments
            .get("input")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Some((capability.clone(), input, client.spec(&capability).cloned()));
    }
    let mut query = world.query::<(&ToolDef, &super::client::CapabilityTool)>();
    let spec = query
        .iter(world)
        .find(|(def, _)| def.0.name.as_str() == tool_name)
        .map(|(_, tool)| tool.spec.clone())?;
    if !client.capabilities.iter().any(|name| name == &spec.name) {
        return None;
    }
    Some((spec.name.clone(), arguments.clone(), Some(spec)))
}

/// The client and generation the call must be sent to.
fn target_client(
    companion_id: &str,
    store: &Store,
    bound: &AgentRole,
    work_id: &str,
) -> Option<(String, u64)> {
    if *bound == AgentRole::Work {
        let session = store.work_session(work_id).ok().flatten()?;
        return Some((session.client_id?, session.activation_revision));
    }
    store
        .activation(companion_id)
        .ok()
        .flatten()
        .and_then(|activation| {
            activation
                .client_id
                .map(|client| (client, activation.revision))
        })
}

fn decide(world: &mut World, call: Entity, outcome: Outcome) {
    if let Some(mut decision) = world.get_mut::<Decision>(call) {
        decision.decide(Decider::plugin("ene-host"), outcome);
    }
}

/// Answers a tool call with a successful result.
pub(crate) fn answer(world: &mut World, call: Entity, text: &str) {
    decide(
        world,
        call,
        Outcome::from(rig_core::tool::ToolResult::success(text.into())),
    );
}

/// Answers a tool call with a refusal.
pub(crate) fn refuse(world: &mut World, call: Entity, reason: &str) {
    decide(world, call, Outcome::refused(reason));
}

/// Approves or denies a pending request. The caller must still have a live
/// tool call (an approval is never replayed onto another one).
pub(crate) fn submit(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> crate::host::error::Result<Approval> {
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval(request_id)?
        .ok_or_else(|| crate::host::error::HostError::not_found("approval not found"))?;
    if approval.status != ApprovalStatus::Pending {
        return Err(crate::host::error::HostError::conflict(
            "approval is no longer pending",
        ));
    }
    // A device approval is bound to the client generation it was issued for:
    // a move voids it instead of applying it to the new client.
    if let (Some(client), Some(revision)) =
        (approval.client_id.as_deref(), approval.activation_revision)
    {
        let current = store
            .activation(&approval.session_id)?
            .and_then(|activation| activation.client_id.map(|id| (id, activation.revision)));
        if current.as_ref() != Some(&(client.to_owned(), revision)) {
            store.resolve_approval(request_id, ApprovalStatus::Expired)?;
            if let Some(hold) = world
                .resource_mut::<Approvals>()
                .by_request
                .remove(request_id)
            {
                if world.get_entity(hold.hold).is_ok() {
                    world.entity_mut(hold.hold).despawn();
                }
                refuse(world, hold.call, "client_moved: the active client moved");
            }
            return Err(crate::host::error::HostError::conflict(
                "client_moved: the approval is bound to an old client generation",
            ));
        }
    }
    let hold = world
        .resource_mut::<Approvals>()
        .by_request
        .remove(request_id);
    let Some(hold) = hold else {
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
        return Err(crate::host::error::HostError::conflict(
            "approval is not attached to a live tool call",
        ));
    };
    let changed = world
        .get::<ToolRequest>(hold.call)
        .map(|request| {
            canonicalise(
                request.call.function.name.as_str(),
                &Value::Object(request.call.function.arguments.clone()),
            )
            .1
        })
        .is_none_or(|hash| hash != approval.action_payload_hash);
    if changed {
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
        let hold_entity = hold.hold;
        if world.get_entity(hold_entity).is_ok() {
            world.entity_mut(hold_entity).despawn();
        }
        refuse(
            world,
            hold.call,
            "the operation arguments changed after the approval was requested",
        );
        return Err(crate::host::error::HostError::conflict(
            "approval no longer matches the call arguments",
        ));
    }
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            world.entity_mut(hold.call).insert(Approved);
            let hold_entity = hold.hold;
            if world.get_entity(hold_entity).is_ok() {
                world.entity_mut(hold_entity).despawn();
            }
            let approval = store.resolve_approval(request_id, ApprovalStatus::Approved)?;
            notify_resolved(world, &approval);
            Ok(approval)
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
            let hold_entity = hold.hold;
            if world.get_entity(hold_entity).is_ok() {
                world.entity_mut(hold_entity).despawn();
            }
            refuse(world, hold.call, "the user denied this operation");
            let approval = store.resolve_approval(request_id, ApprovalStatus::Denied)?;
            notify_resolved(world, &approval);
            Ok(approval)
        }
    }
}

fn notify_resolved(world: &mut World, approval: &Approval) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(
        Some(&approval.session_id),
        "approval_resolved",
        &format!("{}: {}", approval.request_id, approval.status.as_str()),
    );
}

/// A stopped turn takes its held calls with it; the approval must not become
/// reusable.
fn expire_orphaned_approvals(world: &mut World) {
    let orphans: Vec<String> = {
        let approvals = world.resource::<Approvals>();
        approvals
            .by_request
            .iter()
            .filter(|(_, hold)| world.get_entity(hold.call).is_err())
            .map(|(request, _)| request.clone())
            .collect()
    };
    for request in orphans {
        let hold = world
            .resource_mut::<Approvals>()
            .by_request
            .remove(&request);
        if let Some(hold) = hold {
            let hold_entity = hold.hold;
            if world.get_entity(hold_entity).is_ok() {
                world.entity_mut(hold_entity).despawn();
            }
        }
        if let Ok(store) = world.resource::<Store>().approval(&request)
            && store.is_some_and(|approval| approval.status == ApprovalStatus::Pending)
        {
            let store = world.resource::<Store>().clone();
            let _ = store.resolve_approval(&request, ApprovalStatus::Expired);
        }
    }
}

/// A move invalidates approvals bound to the old client generation: the held
/// calls are refused with `client_moved` instead of waiting forever.
pub(crate) fn invalidate_generation(
    world: &mut World,
    companion: &str,
    client: &str,
    revision: u64,
) {
    let requests: Vec<String> = {
        let approvals = world.resource::<Approvals>();
        approvals
            .by_request
            .iter()
            .filter(|(_, hold)| {
                hold.approval.session_id == companion
                    && hold.approval.client_id.as_deref() == Some(client)
                    && hold.approval.activation_revision == Some(revision)
            })
            .map(|(request, _)| request.clone())
            .collect()
    };
    for request in requests {
        let hold = world
            .resource_mut::<Approvals>()
            .by_request
            .remove(&request);
        if let Some(hold) = hold {
            if world.get_entity(hold.hold).is_ok() {
                world.entity_mut(hold.hold).despawn();
            }
            refuse(
                world,
                hold.call,
                "client_moved: the active client moved before the approval was answered",
            );
        }
        let store = world.resource::<Store>().clone();
        if store
            .approval(&request)
            .ok()
            .flatten()
            .is_some_and(|approval| approval.status == ApprovalStatus::Pending)
        {
            let _ = store.resolve_approval(&request, ApprovalStatus::Expired);
        }
    }
}
