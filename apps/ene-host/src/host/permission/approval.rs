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
        let Some(session) = store.work_session(work_id)? else {
            return Ok(None);
        };
        if session.companion_id != approval.session_id {
            return Ok(None);
        }
        return Ok(session.client_id);
    }
    if approval.run_id.is_empty() {
        return Ok(store
            .activation(&approval.session_id)?
            .and_then(|activation| activation.client_id));
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

// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_lines)]
pub(crate) fn submit_with_outcome(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> Result<SubmitOutcome> {
    let store = world.resource::<Store>().clone();
    let approval = store
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
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
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
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
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
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            let call_binding = world.get::<CallBinding>(hold.call).cloned();
            let mut rebound_client: Option<String> = None;
            if let Some(binding) = &call_binding {
                let target_id =
                    bound_client(&store, &approval)?.unwrap_or_else(|| binding.client_id.clone());
                let client = store.client(&target_id)?.ok_or_else(|| {
                    HostError::Conflict(("the bound client is not registered").into())
                })?;
                if !client.is_granted(&binding.capability) {
                    return Err(HostError::Conflict(format!(
                        "capability `{}` is no longer granted on `{target_id}`",
                        binding.capability
                    )));
                }
                if let Some((tool_name, raw_args)) = &request_snapshot {
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
                        store.resolve_approval(request_id, ApprovalStatus::Denied)?;
                        release(world, request_id);
                        refuse(world, hold.call, &reason);
                        return Err(HostError::Permission(reason));
                    }
                    if approval.client_id.as_deref() != Some(target_id.as_str()) {
                        let rules = store.policy_rules()?;
                        if let Verdict::Deny { reason } = decide_action(&rules, &action) {
                            store.resolve_approval(request_id, ApprovalStatus::Denied)?;
                            release(world, request_id);
                            refuse(world, hold.call, &reason);
                            return Err(HostError::Permission(reason));
                        }
                    }
                }
                rebound_client = Some(target_id);
            }
            let approval = store.resolve_approval_with_client(
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
            let mut rule_saved = false;
            let mut rule_error = None;
            if matches!(decision, ApprovalDecision::AlwaysApprove) {
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
                match store.apply_policy_rules(&[rule], &[]) {
                    Ok(rules) => {
                        rule_saved = true;
                        let wire_rules = rules.iter().map(crate::host::wire::policy_rule).collect();
                        let mut notifier = world.resource_mut::<Notifier>();
                        notifier.push(
                            None,
                            None,
                            w::notification::Event::PolicyRulesUpdated(Box::new(
                                w::PolicyRulesUpdated {
                                    rules: wire_rules,
                                    ..Default::default()
                                },
                            )),
                        );
                    }
                    Err(error) => {
                        if error.is_storage() {
                            crate::host::fail(world, error.to_string());
                        }
                        let msg = error.to_string();
                        tracing::warn!(%error, "failed to persist AlwaysApprove policy rule");
                        let mut notifier = world.resource_mut::<Notifier>();
                        notifier.notice(
                            Some(&approval.session_id),
                            "policy_rule_save_failed",
                            &format!(
                                "承認は実行されましたが、常時許可ルールの保存に失敗しました: {msg}"
                            ),
                        );
                        rule_error = Some(msg);
                    }
                }
            }
            notify_resolved(world, &approval);
            Ok(SubmitOutcome {
                approval,
                rule_saved,
                rule_error,
            })
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
            let approval = store.resolve_approval(request_id, ApprovalStatus::Denied)?;
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
        if let Ok(store) = world.resource::<Store>().approval(&request)
            && store.is_some_and(|approval| approval.status == ApprovalStatus::Pending)
        {
            let store = world.resource::<Store>().clone();
            let _ = store.resolve_approval(&request, ApprovalStatus::Expired);
        }
    }
}
