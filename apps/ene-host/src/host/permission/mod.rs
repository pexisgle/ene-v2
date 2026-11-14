//! Approval requests, saved policy rules and the decision the Gate applies.
//!
//! The pipeline follows docs/design/permissions.md §3: hard denial, explicit
//! every-time confirmation, saved rules, then the user. The alpha does not run
//! the auxiliary decision model yet, so an unregistered device operation falls
//! through to the confirmation card (fail-safe).
use super::client::{BUILTIN_TOOLS, CapabilityApproval, CapabilitySpec};
use super::db::{digest, truncate};
use super::error::{Failure, HostError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PolicyEffect {
    Allow,
    Deny,
    Confirm,
}

/// An enum stored and sent as a snake_case name, with its two conversions.
impl PolicyEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Confirm => "confirm",
        }
    }

    /// Reads a stored name back through the same serde spelling the wire uses.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_value(text.into()).ok()
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

    /// Reads a stored name back through the same serde spelling the wire uses.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_value(text.into()).ok()
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
    /// The work session the call belongs to, stored when the gate issues a
    /// work approval so submission does not have to follow the run.
    pub(crate) work_id: Option<String>,
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
/// gate them: they are conversation, memory and work management. The generic
/// device dispatcher is the one builtin that is gated.
pub(crate) fn is_host_tool(tool_name: &str) -> bool {
    tool_name != "device_request" && BUILTIN_TOOLS.contains(&tool_name)
}

/// Whether a call skips the capability pipeline: a built-in host tool, or a
/// tool the host serves itself, such as one an MCP server contributed (the
/// operator configured it, so no client approval applies).
fn host_owned(tool_name: &str, host_tools: &std::collections::HashSet<String>) -> bool {
    is_host_tool(tool_name) || host_tools.contains(tool_name)
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
    let normalized = canonical_json(arguments);
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
            description: "files".into(),
            input_schema: r#"{"type":"object"}"#.into(),
            approval,
            ..Default::default()
        }
    }

    #[test]
    fn hard_denied_and_undeclared_operations_fail_safe() {
        let denied = decide_capability(
            &[],
            Some(&spec(CapabilityApproval::Auto)),
            "payment.transfer",
            "device_request",
            &json!({}),
        );
        assert!(matches!(denied, Verdict::Deny { .. }));
        let undeclared =
            decide_capability(&[], None, "device.screen", "device_request", &json!({}));
        assert!(matches!(undeclared, Verdict::Confirm { .. }));
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
    fn rules_decide_by_precedence() {
        let allow = rule(PolicyEffect::Allow, "device_request", "*");
        let confirm = rule(PolicyEffect::Confirm, "device_request", "/tmp/x");
        let verdict = decide_capability(
            &[allow.clone(), confirm],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert!(matches!(verdict, Verdict::Confirm { .. }));
        let allow_only = decide_capability(
            std::slice::from_ref(&allow),
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/y"}),
        );
        assert_eq!(allow_only, Verdict::Allow);
        let wildcard = decide_capability(
            &[rule(PolicyEffect::Confirm, "*", "*")],
            Some(&spec(CapabilityApproval::Auto)),
            "body.motion",
            "body_motion",
            &json!({"motion": "nod"}),
        );
        assert!(matches!(wildcard, Verdict::Confirm { .. }));
        let mut deny = rule(PolicyEffect::Deny, "device_request", "*");
        deny.id = "r2".into();
        let verdict = decide_capability(
            &[rule(PolicyEffect::Confirm, "device_request", "*"), deny],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
        // A saved allow bypasses a capability's default confirmation, but not
        // a deny or a confirm rule.
        let verdict = decide_capability(
            &[allow],
            Some(&spec(CapabilityApproval::Confirm)),
            "device.files",
            "device_request",
            &json!({"path": "/tmp/x"}),
        );
        assert_eq!(verdict, Verdict::Allow);
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
    let host_tools: std::collections::HashSet<String> = world
        .get_resource::<crate::host::resources::HostToolNames>()
        .map_or_else(Default::default, |names| names.0.clone());
    for (call, tool_name, arguments, agent) in calls {
        // Built-in host tools and the tools an MCP server contributed belong to
        // the host process; the operator configured them.
        if host_owned(&tool_name, &host_tools) {
            continue;
        }
        let (companion_id, work_id, run_id) = binding_of(world, agent);
        let Some((client_id, activation_revision)) =
            target_client(&companion_id, &store, work_id.as_deref())
        else {
            refuse(
                world,
                call,
                "no_active_client: no client is bound to this agent",
            );
            continue;
        };
        let Some(client) = store.client(&client_id).ok().flatten() else {
            refuse(world, call, "the bound client is not registered");
            continue;
        };
        // A tool that is neither a host tool nor a declared capability is
        // refused: a future tool must opt into one of the two paths.
        let resolved = match resolve_capability(
            world,
            agent,
            &client,
            activation_revision,
            &tool_name,
            &arguments,
        ) {
            Ok(resolved) => resolved,
            Err(error) => {
                refuse(world, call, error.message());
                continue;
            }
        };
        let Some((capability, input, spec)) = resolved else {
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
        let verdict = decide_capability(&rules, spec.as_ref(), &capability, &tool_name, &input);
        world.entity_mut(call).insert(CallBinding {
            client_id: client_id.clone(),
            activation_revision,
            capability: capability.clone(),
        });
        match verdict {
            Verdict::Allow => {}
            Verdict::Deny { reason } => refuse(world, call, &reason),
            Verdict::Confirm { target } => {
                let hold = world.spawn(HoldOf(call)).id();
                let request_id = uuid::Uuid::new_v4().to_string();
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    work_id: work_id.clone(),
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
                notifier.notice(
                    Some(&companion_id),
                    "approval_required",
                    &format!("{target}: approval is required before this operation runs"),
                );
            }
        }
    }
}

/// The companion, work session and run an agent serves. A work agent carries
/// both ids; a companion agent leaves the work ones empty.
fn binding_of(world: &World, agent: Entity) -> (String, Option<String>, String) {
    if let Some(companion) = world.get::<super::agent::CompanionAgent>(agent) {
        return (companion.companion_id.clone(), None, String::new());
    }
    match world.get::<super::agent::WorkAgent>(agent) {
        Some(work) => (
            work.companion_id.clone(),
            Some(work.work_id.clone()),
            work.run_id.clone(),
        ),
        None => (String::new(), None, String::new()),
    }
}

/// The capability a device tool call names, with its input and declaration.
/// The client is already resolved, so the hand resolves against the client it
/// is bound to, not against the companion's current activation. A capability
/// tool is parent-only and must be published for the calling companion's
/// bound generation; `client::dynamic::tool_spec_for` resolves that binding
/// per agent, so one published definition can serve several companions. A
/// call that no longer matches is refused with a reason instead of being
/// treated as an unknown tool.
fn resolve_capability(
    world: &mut World,
    agent: Entity,
    client: &Client,
    activation_revision: u64,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<(String, Value, Option<CapabilitySpec>)>> {
    if tool_name == "device_request" {
        let Some(capability) = arguments.get("capability").and_then(Value::as_str) else {
            return Ok(None);
        };
        if !client.capabilities.iter().any(|name| name == capability) {
            return Ok(None);
        }
        let input = arguments
            .get("input")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Ok(Some((
            capability.to_owned(),
            input,
            client.spec(capability).cloned(),
        )));
    }
    // Declared capability tools are offered to the parent only; the hand uses
    // the generic dispatcher so its work-session binding stays explicit.
    if world.get::<super::agent::WorkAgent>(agent).is_some() {
        return Err(HostError::permission(
            "capability tools are offered to the parent only",
        ));
    }
    let Some(spec) =
        super::client::dynamic::tool_spec_for(world, client, activation_revision, tool_name)?
    else {
        return Ok(None);
    };
    Ok(Some((spec.name.clone(), arguments.clone(), Some(spec))))
}

/// The client and generation the call must be sent to.
fn target_client(
    companion_id: &str,
    store: &Store,
    work_id: Option<&str>,
) -> Option<(String, u64)> {
    store.target(companion_id, work_id).ok().flatten()
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

/// Takes a held call out of the map and despawns its hold marker.
fn release(world: &mut World, request: &str) -> Option<Hold> {
    let hold = world
        .resource_mut::<Approvals>()
        .by_request
        .remove(request)?;
    if world.get_entity(hold.hold).is_ok() {
        world.entity_mut(hold.hold).despawn();
    }
    Some(hold)
}

/// The client generation an approval is bound to. The head follows the
/// companion's activation. The hand follows the work session stored with the
/// approval; only an approval row from before the column existed (or one
/// inserted without it) is resolved through its run.
fn bound_generation(store: &Store, approval: &Approval) -> Result<Option<(String, u64)>> {
    if let Some(work_id) = approval.work_id.as_deref().filter(|id| !id.is_empty()) {
        let Some(session) = store.work_session(work_id)? else {
            return Ok(None);
        };
        if session.companion_id != approval.session_id {
            return Ok(None);
        }
        return Ok(session
            .client_id
            .map(|client| (client, session.activation_revision)));
    }
    if approval.run_id.is_empty() {
        return Ok(store
            .activation(&approval.session_id)?
            .and_then(|activation| {
                activation
                    .client_id
                    .map(|client| (client, activation.revision))
            }));
    }
    let Some(run) = store.run(&approval.run_id)? else {
        return Ok(None);
    };
    let Some(session) = store.work_session(&run.work_id)? else {
        return Ok(None);
    };
    if session.companion_id != approval.session_id {
        return Ok(None);
    }
    Ok(session
        .client_id
        .map(|client| (client, session.activation_revision)))
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
        .ok_or_else(|| HostError::not_found("approval not found"))?;
    if approval.status != ApprovalStatus::Pending {
        return Err(HostError::conflict("approval is no longer pending"));
    }
    super::compaction::note_user_activity(world);
    // A device approval is bound to the client generation it was issued for:
    // a move voids it instead of applying it to the new client. A work
    // approval is bound to the work session's generation, which does not
    // follow the companion's activation.
    if let (Some(client), Some(revision)) =
        (approval.client_id.as_deref(), approval.activation_revision)
    {
        let current = bound_generation(&store, &approval)?;
        if current.as_ref() != Some(&(client.to_owned(), revision)) {
            store.resolve_approval(request_id, ApprovalStatus::Expired)?;
            if let Some(hold) = release(world, request_id) {
                refuse(world, hold.call, "client_moved: the active client moved");
            }
            return Err(HostError::conflict(
                "client_moved: the approval is bound to an old client generation",
            ));
        }
    }
    let Some(hold) = release(world, request_id) else {
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
        return Err(HostError::conflict(
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
        refuse(
            world,
            hold.call,
            "the operation arguments changed after the approval was requested",
        );
        return Err(HostError::conflict(
            "approval no longer matches the call arguments",
        ));
    }
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            world.entity_mut(hold.call).insert(Approved);
            let approval = store.resolve_approval(request_id, ApprovalStatus::Approved)?;
            notify_resolved(world, &approval);
            Ok(approval)
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
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
        release(world, &request);
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
        if let Some(hold) = release(world, &request) {
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

/// RFC 8785 (JCS) canonical JSON: the approval hash must not depend on
/// object key order. serde_json_canonicalizer sorts keys by UTF-16 code
/// units and uses the standard number form, so the host does not carry a
/// sorting serializer of its own.
fn canonical_json(value: &Value) -> String {
    serde_json_canonicalizer::to_string(value).unwrap_or_else(|_| value.to_string())
}
