//! Approval submission, hold release, rule persistence, and orphan expiration.
use super::gate::{Approvals, Approved, CallBinding, Hold, ResolveApprovalArgs, SubmitOutcome};
use crate::host::{
    db::Store,
    error::{HostError, Result},
    notify::Notifier,
    resources::CompanionAgent,
};
use bevy_ecs::prelude::*;
use ene_policy::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, PolicyEffect, PolicyRule, Verdict,
    canonicalise, decide_action, extract_target, hard_denied_action, hex_digest,
};
use ene_rpc as w;
use rig_ecs::prelude::*;
use serde_json::Value;

pub(crate) fn decide(world: &mut World, call: Entity, outcome: Outcome) {
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

/// The client an approval is bound to.
fn bound_client(store: &Store, approval: &Approval) -> Result<Option<String>> {
    if let Some(work_id) = approval.work_id.as_deref().filter(|id| !id.is_empty()) {
        let Some(session) = store.work_repo().work_session(work_id)? else {
            return Ok(None);
        };
        if session.companion_id != approval.session_id {
            return Ok(None);
        }
        return Ok(session.client_id);
    }
    if approval.run_id.is_empty() {
        return Ok(store
            .client_repo()
            .activation(&approval.session_id)?
            .and_then(|activation| activation.client_id));
    }
    let Some(run) = store.work_repo().run(&approval.run_id)? else {
        return Ok(None);
    };
    let Some(session) = store.work_repo().work_session(&run.work_id)? else {
        return Ok(None);
    };
    if session.companion_id != approval.session_id {
        return Ok(None);
    }
    Ok(session.client_id)
}

/// Approves or denies a pending request. Validates hash, client binding,
/// grants, and DB state before mutating the ECS hold or granting execution.
pub(crate) fn submit(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> Result<Approval> {
    submit_with_outcome(world, request_id, decision).map(|outcome| outcome.approval)
}

fn load_verified_hold(
    world: &mut World,
    store: &Store,
    request_id: &str,
) -> Result<(Hold, Option<(String, Value)>)> {
    let approval = store
        .approval_repo()
        .approval(request_id)?
        .ok_or_else(|| HostError::NotFound(("approval not found").into()))?;
    if approval.status != ApprovalStatus::Pending {
        return Err(HostError::Conflict(
            ("approval is no longer pending").into(),
        ));
    }
    let Some(hold) = world
        .resource::<Approvals>()
        .by_request
        .get(request_id)
        .copied()
    else {
        store
            .approval_repo()
            .resolve_approval(request_id, ApprovalStatus::Expired)?;
        return Err(HostError::Conflict(
            ("approval is not attached to a live tool call").into(),
        ));
    };
    let request_snapshot = world.get::<ToolRequest>(hold.call).map(|request| {
        (
            request.call.function.name.as_str().to_owned(),
            Value::Object(request.call.function.arguments.clone()),
        )
    });
    let changed = request_snapshot
        .as_ref()
        .map(|(name, args)| canonicalise(name, args).1)
        .is_none_or(|hash| hash != approval.action_payload_hash);
    if changed {
        store
            .approval_repo()
            .resolve_approval(request_id, ApprovalStatus::Expired)?;
        release(world, request_id);
        refuse(
            world,
            hold.call,
            "the operation arguments changed after the approval was requested",
        );
        return Err(HostError::Conflict(
            ("approval no longer matches the call arguments").into(),
        ));
    }
    Ok((hold, request_snapshot))
}

fn validate_bound_approval_client(
    world: &mut World,
    store: &Store,
    request_id: &str,
    hold: Hold,
    request_snapshot: Option<&(String, Value)>,
) -> Result<Option<String>> {
    let Some(binding) = world.get::<CallBinding>(hold.call).cloned() else {
        return Ok(None);
    };
    let approval = store
        .approval_repo()
        .approval(request_id)?
        .ok_or_else(|| HostError::NotFound(("approval not found").into()))?;
    let target_id = bound_client(store, &approval)?.unwrap_or_else(|| binding.client_id.clone());
    let client = store
        .client_repo()
        .client(&target_id)?
        .ok_or_else(|| HostError::Conflict(("the bound client is not registered").into()))?;
    if !client.is_granted(&binding.capability) {
        return Err(HostError::Conflict(format!(
            "capability `{}` is no longer granted on `{target_id}`",
            binding.capability
        )));
    }
    if let Some((tool_name, raw_args)) = request_snapshot {
        let empty = serde_json::json!({});
        let input = if tool_name == "device_request" {
            raw_args.get("input").unwrap_or(&empty).clone()
        } else {
            raw_args.clone()
        };
        let action = CandidateAction::DeviceCapability {
            capability: binding.capability.clone(),
            spec: client.spec(&binding.capability).cloned().map(Box::new),
            tool_name: tool_name.clone(),
            input,
            raw_arguments: raw_args.clone(),
        };
        let typed_target = extract_target(&action);
        if let Some(reason) = hard_denied_action(&action, &typed_target) {
            store
                .approval_repo()
                .resolve_approval(request_id, ApprovalStatus::Denied)?;
            release(world, request_id);
            refuse(world, hold.call, &reason);
            return Err(HostError::Permission(reason));
        }
        if approval.client_id.as_deref() != Some(target_id.as_str()) {
            let rules = store.policy_repo().policy_rules()?;
            if let Verdict::Deny { reason } = decide_action(&rules, &action) {
                store
                    .approval_repo()
                    .resolve_approval(request_id, ApprovalStatus::Denied)?;
                release(world, request_id);
                refuse(world, hold.call, &reason);
                return Err(HostError::Permission(reason));
            }
        }
    }
    Ok(Some(target_id))
}

fn persist_always_allow_rule(
    world: &mut World,
    store: &Store,
    approval: &Approval,
) -> (bool, Option<String>) {
    let pattern = if approval.normalized_arguments.trim().is_empty() {
        "*".to_owned()
    } else if approval.normalized_arguments.chars().count() <= 4_000 {
        approval.normalized_arguments.clone()
    } else {
        format!("exact_sha256:{}", hex_digest(&approval.action_payload_hash))
    };
    let rule = PolicyRule {
        id: uuid::Uuid::new_v4().to_string(),
        tool_name: approval.tool_name.clone(),
        pattern,
        effect: PolicyEffect::Allow,
        created_at: String::new(),
    };
    match store.policy_repo().apply_policy_rules(&[rule], &[]) {
        Ok(rules) => {
            let wire_rules = rules.iter().map(crate::host::wire::policy_rule).collect();
            world.resource_mut::<Notifier>().push(
                None,
                None,
                w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
                    rules: wire_rules,
                    ..Default::default()
                })),
            );
            (true, None)
        }
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            let msg = error.to_string();
            tracing::warn!(%error, "failed to persist AlwaysApprove policy rule");
            world.resource_mut::<Notifier>().notice(
                Some(&approval.session_id),
                "policy_rule_save_failed",
                &format!("承認は実行されましたが、常時許可ルールの保存に失敗しました: {msg}"),
            );
            (false, Some(msg))
        }
    }
}

pub(crate) fn submit_with_outcome(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> Result<SubmitOutcome> {
    let store = world.resource::<Store>().clone();
    let (hold, request_snapshot) = load_verified_hold(world, &store, request_id)?;
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            let rebound_client = validate_bound_approval_client(
                world,
                &store,
                request_id,
                hold,
                request_snapshot.as_ref(),
            )?;
            let approval = store.approval_repo().resolve_approval_with_client(
                request_id,
                ApprovalStatus::Approved,
                rebound_client.as_deref(),
            )?;
            release(world, request_id);
            if let Some(target_id) = rebound_client
                && let Some(mut binding) = world.get_mut::<CallBinding>(hold.call)
            {
                binding.client_id = target_id;
            }
            world.entity_mut(hold.call).insert(Approved);
            let (rule_saved, rule_error) = if matches!(decision, ApprovalDecision::AlwaysApprove) {
                persist_always_allow_rule(world, &store, &approval)
            } else {
                (false, None)
            };
            notify_resolved(world, &approval);
            Ok(SubmitOutcome {
                approval,
                rule_saved,
                rule_error,
            })
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
            let approval = store
                .approval_repo()
                .resolve_approval(request_id, ApprovalStatus::Denied)?;
            release(world, request_id);
            refuse(world, hold.call, "the user denied this operation");
            notify_resolved(world, &approval);
            Ok(SubmitOutcome {
                approval,
                rule_saved: false,
                rule_error: None,
            })
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

/// The head's `resolve_approval` tool: answers one pending approval of its own companion.
pub(crate) fn resolve_approval(
    world: &mut World,
    agent: Entity,
    _call_id: &str,
    args: ResolveApprovalArgs,
) -> Result<String> {
    let companion_id = world
        .get::<CompanionAgent>(agent)
        .map(|companion| companion.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))?;
    let decision = match args.decision.trim() {
        "approve" => ApprovalDecision::Approve,
        "deny" => ApprovalDecision::Deny,
        other => {
            return Err(HostError::Invalid(format!(
                "decision must be `approve` or `deny`, not `{other}`"
            )));
        }
    };
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval_repo()
        .approval(&args.request_id)?
        .ok_or_else(|| HostError::NotFound(("approval not found").into()))?;
    if approval.session_id != companion_id {
        return Err(HostError::Permission(
            ("this approval belongs to another companion").into(),
        ));
    }
    let approval = submit(world, &args.request_id, decision)?;
    Ok(serde_json::json!({
        "request_id": approval.request_id,
        "status": approval.status.as_str(),
    })
    .to_string())
}

/// A stopped turn takes its held calls with it; the approval must not become reusable.
pub(super) fn expire_orphaned_approvals(world: &mut World) {
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
        if let Ok(store) = world.resource::<Store>().approval_repo().approval(&request)
            && store.is_some_and(|approval| approval.status == ApprovalStatus::Pending)
        {
            let store = world.resource::<Store>().clone();
            let _ = store
                .approval_repo()
                .resolve_approval(&request, ApprovalStatus::Expired);
        }
    }
}
