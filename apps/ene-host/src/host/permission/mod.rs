//! Approval holds, the Stage::Gate policy check and the policy/approval surface.
//!
//! A live tool call waits on a [`HoldOf`] entity carrying [`ApprovalHold`]; an
//! approved call carries [`Approved`] and consumes its request once. The
//! component lifecycle, rather than a side table, expires requests when a turn
//! stops, which also leaves room for a future OS sandbox or decision engine.
use super::{
    agent::AgentKey,
    db::{Result, Store},
    error::HostError,
    model::{Binding, Refusal},
    notify::Notifier,
    resources::HostState,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::{Value, json};

pub(crate) fn gate(world: &mut World) {
    let calls: Vec<Entity> = world
        .query_filtered::<Entity, ReadyToStart>()
        .iter(world)
        .collect();
    for entity in calls {
        if world
            .get::<Decision>(entity)
            .is_some_and(Decision::is_decided)
        {
            continue;
        }
        let ready = world.resource::<HostState>().ready().err();
        let needs_binding = world.get::<ModelRequest>(entity).is_some()
            || world.get::<ToolRequest>(entity).is_some();
        let permitted = if needs_binding {
            world
                .get::<Binding>(entity)
                .or_else(|| {
                    world
                        .get::<ForAgent>(entity)
                        .and_then(|agent| world.get::<Binding>(agent.0))
                })
                .is_some_and(|binding| binding.permit.load(std::sync::atomic::Ordering::Acquire))
        } else {
            true
        };
        let tool_request = world.get::<ToolRequest>(entity).map(|request| {
            (
                request.call.function.name.to_string(),
                serde_json::to_string(&request.call.function.arguments).unwrap_or_default(),
            )
        });
        let policy = match (
            tool_request.as_ref(),
            world.get::<Approved>(entity).cloned(),
        ) {
            (_, Some(mut approved)) if !approved.consumed => {
                let store = world.resource::<Store>().clone();
                match store.consume_approval(&approved.request_id) {
                    Ok(true) => {
                        approved.consumed = true;
                        world.entity_mut(entity).insert(approved);
                        Policy::Allow
                    }
                    Ok(false) => Policy::Refuse("approval was already consumed".into()),
                    Err(error) => Policy::Refuse(format!("policy unavailable: {error}")),
                }
            }
            (_, Some(_)) => Policy::Allow,
            (Some((name, args)), None) => match policy_refusal(world, entity, name, args) {
                Ok(Policy::Allow) => Policy::Allow,
                Ok(Policy::Hold { reason }) => Policy::Hold { reason },
                Ok(Policy::Refuse(reason)) => Policy::Refuse(reason),
                Err(error) => Policy::Refuse(format!("policy unavailable: {error}")),
            },
            (None, None) => Policy::Allow,
        };
        if matches!(&policy, Policy::Hold { .. }) {
            continue;
        }
        let policy = match policy {
            Policy::Refuse(reason) => Some(reason),
            _ => None,
        };
        let refusal = if let Some(policy) = policy {
            Some(policy)
        } else if let Some(refusal) = world.get::<Refusal>(entity) {
            Some(format!("model unavailable: {}", refusal.0))
        } else if permitted {
            None
        } else {
            Some("model permission changed".into())
        };
        let Some(mut decision) = world.get_mut::<Decision>(entity) else {
            continue;
        };
        if decision.is_decided() {
            continue;
        }
        if let Some(error) = ready {
            decision.decide(
                Decider::plugin("ene-host"),
                Outcome::refused(error.to_string()),
            );
        } else if let Some(why) = refusal {
            tracing::warn!(call = ?entity, %why, "a call was refused");
            decision.decide(Decider::plugin("ene-host"), Outcome::refused(why));
        }
    }
}

enum Policy {
    Allow,
    Refuse(String),
    Hold { reason: String },
}

/// A live tool call waits for this approval request. The component sits on the
/// [`HoldOf`] entity, so stopping the turn releases the call and expires the
/// request with it.
#[derive(Component, Clone)]
pub(crate) struct ApprovalHold {
    pub(crate) request_id: String,
}

/// The user approved this exact call. Gate consumes the request and lets the
/// call run; the request's hash already binds it to the call's arguments.
#[derive(Component, Clone)]
pub(crate) struct Approved {
    pub(crate) request_id: String,
    consumed: bool,
}

impl Approved {
    pub(crate) fn new(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            consumed: false,
        }
    }
}

pub(crate) fn hold_released(
    released: On<Despawn<ApprovalHold>>,
    holds: Query<&ApprovalHold>,
    mut commands: Commands,
) {
    let Ok(hold) = holds.get(released.entity) else {
        return;
    };
    let request_id = hold.request_id.clone();
    commands.queue(move |world: &mut World| {
        // A successor takes over the hold before the old entity goes. Expiring
        // the request then would invalidate the approval the successor waits on.
        if world
            .query::<&ApprovalHold>()
            .iter(world)
            .any(|hold| hold.request_id == request_id)
        {
            return;
        }
        let store = world.resource::<Store>().clone();
        match store.approval(&request_id) {
            Ok(Some(approval)) if approval.status == "pending" => {
                if let Err(error) = store.set_approval_status(&request_id, "expired")
                    && error.is_storage()
                {
                    super::fail(world, error.to_string());
                }
            }
            Ok(_) => {}
            Err(error) if error.is_storage() => super::fail(world, error.to_string()),
            Err(_) => {}
        }
    });
}

fn ensure_hold(world: &mut World, request_id: &str, call: Entity) {
    let (existing, stale): (bool, Vec<Entity>) = {
        let mut existing = false;
        let mut stale = Vec::new();
        for (hold, ApprovalHold { request_id: found }, HoldOf(held)) in world
            .query::<(Entity, &ApprovalHold, &HoldOf)>()
            .iter(world)
        {
            if found != request_id {
                continue;
            }
            if *held == call {
                existing = true;
            } else {
                stale.push(hold);
            }
        }
        (existing, stale)
    };
    if existing {
        return;
    }
    // Spawn the replacement before despawning stale holds: the despawn
    // observer only expires a request with no live hold left.
    world.spawn((
        HoldOf(call),
        ApprovalHold {
            request_id: request_id.to_owned(),
        },
    ));
    for hold in stale {
        world.despawn(hold);
    }
}

fn policy_refusal(world: &mut World, entity: Entity, name: &str, args: &str) -> Result<Policy> {
    let store = world.resource::<Store>().clone();
    let rules = store.policy_rules()?;
    let matches = |rule: &w::PolicyRule| rule_matches(rule, name, args);
    if rules
        .iter()
        .any(|rule| rule.effect == "deny" && matches(rule))
    {
        return Ok(Policy::Refuse("policy denied".into()));
    }
    if !rules
        .iter()
        .any(|rule| rule.effect == "confirm" && matches(rule))
    {
        return Ok(Policy::Allow);
    }
    let hash = crate::host::db::digest(format!("{name}:{args}").as_bytes());
    let agent = world
        .get::<ForAgent>(entity)
        .map(|agent| agent.0)
        .ok_or_else(|| HostError::permission("unbound tool call"))?;
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    let session_id = key.agent_id();
    let run_id = world
        .get::<CallOf>(entity)
        .and_then(|CallOf(turn)| world.get::<Turn>(*turn))
        .map(|turn| format!("{}:{}", session_id, turn.start.get()))
        .unwrap_or_default();
    if let Some(existing) = store.approval_for(name, hash.as_bytes(), &session_id, &run_id)? {
        match existing.status.as_str() {
            "approved" => {
                if store.consume_approval(&existing.request_id)? {
                    return Ok(Policy::Allow);
                }
                return Ok(Policy::Refuse("approval was already consumed".into()));
            }
            "pending" => {
                ensure_hold(world, &existing.request_id, entity);
                return Ok(Policy::Hold {
                    reason: "approval pending".into(),
                });
            }
            "denied" => return Ok(Policy::Refuse("approval denied".into())),
            _ => {}
        }
    }
    let request = w::ApprovalRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id,
        run_id,
        action_payload_hash: hash.as_bytes().to_vec(),
        tool_name: name.into(),
        normalized_arguments: args.into(),
        status: "pending".into(),
        created_at: crate::host::db::now(),
        ..Default::default()
    };
    store.save_approval(&request)?;
    ensure_hold(world, &request.request_id, entity);
    world.resource_mut::<Notifier>().push(
        Some(key.companion().into()),
        None,
        w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
            request: request.clone().into(),
            ..Default::default()
        })),
    );
    Ok(Policy::Hold {
        reason: "approval required".into(),
    })
}

pub(crate) fn policy_rules(store: &Store) -> Result<w::PolicyRules> {
    Ok(w::PolicyRules {
        items: store.policy_rules()?,
        ..Default::default()
    })
}

pub(crate) fn update_policy_rules(
    world: &mut World,
    request: w::UpdatePolicyRulesRequest,
) -> Result<w::PolicyRules> {
    let store = world.resource::<Store>().clone();
    store.update_policy_rules(&request.add_or_update, &request.remove_ids)?;
    let rules = policy_rules(&store)?;
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
            rules: rules.items.clone(),
            ..Default::default()
        })),
    );
    Ok(rules)
}

pub(crate) fn pending_approvals(store: &Store) -> Result<w::PendingApprovals> {
    Ok(w::PendingApprovals {
        items: store.pending_approvals()?,
        ..Default::default()
    })
}

pub(crate) fn submit_approval(
    world: &mut World,
    request: w::SubmitApprovalRequest,
) -> Result<w::SubmitApprovalResponse> {
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval(&request.request_id)?
        .ok_or_else(|| HostError::not_found("approval request not found"))?;
    if approval.status != "pending" {
        return Err(HostError::conflict("approval is already resolved"));
    }
    let live = {
        let mut holds = world.query::<(Entity, &ApprovalHold, &HoldOf)>();
        holds.iter(world).find_map(|(entity, hold, HoldOf(call))| {
            (hold.request_id == request.request_id).then_some((entity, *call))
        })
    };
    let Some((hold, call)) = live else {
        return Err(HostError::conflict(
            "approval is not attached to a live tool call",
        ));
    };
    if world.get_entity(call).is_err() {
        return Err(HostError::conflict(
            "approval is not attached to a live tool call",
        ));
    }
    let status = match request.decision.as_str() {
        "approve" | "always_approve" => "approved",
        "deny" => "denied",
        _ => return Err(HostError::invalid("invalid approval decision")),
    };
    // Every approval today comes from an explicit `confirm` rule, which stays
    // authoritative (design/permissions.md §5.B). "always_approve" therefore
    // resolves like "approve" and stores no rule; the automatic decision
    // pipeline that could produce other approvals is not implemented.
    store.set_approval_status(&request.request_id, status)?;
    match status {
        "approved" => {
            world
                .entity_mut(call)
                .insert(Approved::new(request.request_id.clone()));
        }
        "denied" => {
            if let Some(mut decision) = world.get_mut::<rig_ecs::stage::Decision>(call) {
                decision.decide(Decider::Person, Outcome::refused("user denied"));
            }
        }
        _ => {}
    }
    if world.get_entity(hold).is_ok() {
        world.despawn(hold);
    }
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "approval_resolved".into(),
            payload: json!({
                "request_id": request.request_id,
                "status": status,
                "reason": request.reason,
            })
            .to_string(),
            ..Default::default()
        })),
    );
    Ok(w::SubmitApprovalResponse {
        acknowledged: true,
        status: status.into(),
        ..Default::default()
    })
}

/// Whether `rule` applies to a call of `tool` with `args`, the normalized
/// argument JSON.
///
/// A non-empty `pattern` is matched exactly, never as a substring: the whole
/// normalized arguments or one of their string values (a path, URL, command,
/// and so on). A saved `(tool, target)` rule therefore cannot apply to a
/// different target that merely contains the same text.
pub(crate) fn rule_matches(rule: &w::PolicyRule, tool: &str, args: &str) -> bool {
    if !(rule.tool_name.is_empty() || rule.tool_name == "*" || rule.tool_name == tool) {
        return false;
    }
    if rule.pattern.is_empty() {
        return true;
    }
    if rule.pattern == args {
        return true;
    }
    serde_json::from_str::<Value>(args).is_ok_and(|value| contains_string(&value, &rule.pattern))
}

fn contains_string(value: &Value, pattern: &str) -> bool {
    match value {
        Value::String(text) => text == pattern,
        Value::Array(items) => items.iter().any(|item| contains_string(item, pattern)),
        Value::Object(object) => object.values().any(|value| contains_string(value, pattern)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_patterns_match_exactly_not_as_substrings() {
        let rule = w::PolicyRule {
            tool_name: "device_request".into(),
            pattern: "x".into(),
            effect: "confirm".into(),
            ..Default::default()
        };
        assert!(rule_matches(
            &rule,
            "device_request",
            r#"{"capability":"files","input":{"path":"x"}}"#
        ));
        assert!(!rule_matches(
            &rule,
            "device_request",
            r#"{"capability":"files","input":{"path":"x-long"}}"#
        ));
        assert!(!rule_matches(
            &rule,
            "other_tool",
            r#"{"capability":"files","input":{"path":"x"}}"#
        ));
    }
}
