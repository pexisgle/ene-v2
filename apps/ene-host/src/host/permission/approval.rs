//! Approval submission, hold release, rule persistence, and orphan expiration.
use super::{
    decision::Decisions,
    gate::{Approvals, Approved, CallBinding, ResolveApprovalArgs},
};
use crate::host::{
    db::Store,
    error::{HostError, Result},
    notify::Notifier,
    resources::CompanionAgent,
    web_search::{self, SearchBinding, SearchConfigError},
};
use bevy_ecs::prelude::*;
use ene_policy::{
    Approval, ApprovalDecision, ApprovalStatus, PolicyEffect, PolicyRule, Verdict,
    approved_search_pattern, canonicalise, decide_action, decide_capability, hex_digest,
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

/// Removes a held call and despawns its hold marker.
fn release(world: &mut World, request: &str) {
    if let Some(hold) = world.resource_mut::<Approvals>().by_request.remove(request) {
        let _despawned = world.try_despawn(hold.hold);
    }
    if let Some(mut decisions) = world.get_resource_mut::<Decisions>() {
        decisions.running.remove(request);
    }
}

/// The client an approval is bound to.
fn bound_client(store: &Store, approval: &Approval) -> Result<Option<String>> {
    let work = store.work_repo();
    let run;
    let work_id = match approval.work_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => id,
        None if approval.run_id.is_empty() => {
            return Ok(store
                .client_repo()
                .activation(&approval.session_id)?
                .and_then(|activation| activation.client_id));
        }
        None => {
            let Some(saved) = work.run(&approval.run_id)? else {
                return Ok(None);
            };
            run = saved;
            &run.work_id
        }
    };
    Ok(work
        .work_session(work_id)?
        .filter(|session| session.companion_id.as_str() == approval.session_id)
        .and_then(|session| session.client_id))
}

#[derive(Clone, Copy)]
pub(crate) enum ApprovalRequest<'a> {
    Operator(&'a str),
    Companion {
        request: &'a str,
        companion: &'a str,
    },
    DecisionModel {
        request: &'a str,
        companion: &'a str,
    },
}

#[derive(Debug)]
pub(crate) enum RuleSave {
    NotRequested,
    Saved,
    Failed(String),
}

pub(crate) struct SubmitOutcome {
    pub(crate) approval: Approval,
    pub(crate) rule: RuleSave,
}

#[cfg(test)]
pub(crate) fn submit(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> Result<Approval> {
    submit_with_outcome(world, ApprovalRequest::Operator(request_id), decision)
        .map(|outcome| outcome.approval)
}

enum BoundClientCheck {
    Allowed(Option<String>),
    Denied(String),
    RequiresUser,
}

fn validate_bound_approval_client(
    store: &Store,
    approval: &Approval,
    binding: Option<&CallBinding>,
    tool_name: &str,
    raw_arguments: &Value,
    automated: bool,
) -> Result<BoundClientCheck> {
    let Some(binding) = binding else {
        return Ok(BoundClientCheck::Allowed(None));
    };
    let target_id = bound_client(store, approval)?.unwrap_or_else(|| binding.client_id.clone());
    let client = store
        .client_repo()
        .client(&target_id)?
        .ok_or_else(|| HostError::Conflict("the bound client is not registered".into()))?;
    if !client.is_granted(&binding.capability) {
        return Err(HostError::Conflict(format!(
            "capability `{}` is no longer granted on `{target_id}`",
            binding.capability
        )));
    }
    let verdict = decide_capability(
        &store.policy_repo().policy_rules()?,
        client.spec(&binding.capability),
        &binding.capability,
        tool_name,
        raw_arguments,
    );
    Ok(match verdict {
        Verdict::Deny { reason } => BoundClientCheck::Denied(reason),
        Verdict::HumanRequired { .. } if automated => BoundClientCheck::RequiresUser,
        _ => BoundClientCheck::Allowed(Some(target_id)),
    })
}

fn persist_always_allow_rule(
    store: &Store,
    notifier: &mut Notifier,
    approval: &Approval,
    pattern: Option<String>,
) -> Result<()> {
    let pattern = pattern.unwrap_or_else(|| {
        if approval.normalized_arguments.trim().is_empty() {
            "*".to_owned()
        } else if approval.normalized_arguments.chars().count() <= 4_000 {
            approval.normalized_arguments.clone()
        } else {
            format!("exact_sha256:{}", hex_digest(&approval.action_payload_hash))
        }
    });
    let rule = PolicyRule {
        id: uuid::Uuid::new_v4().to_string(),
        tool_name: approval.tool_name.clone(),
        pattern,
        effect: PolicyEffect::Allow,
        created_at: String::new(),
    };
    let rules = store.policy_repo().apply_policy_rules(&[rule], &[])?;
    notifier.push(
        None,
        None,
        w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
            rules: rules.iter().map(Into::into).collect(),
            ..Default::default()
        })),
    );
    Ok(())
}

fn search_approval_pattern(
    binding: Option<&SearchBinding>,
    store: &Store,
    credentials: Option<&crate::host::model::Credentials>,
    providers: &crate::host::providers::Providers,
    arguments: &Value,
    automated: bool,
) -> Result<Option<String>> {
    let changed = || HostError::Conflict(SearchConfigError::ConnectionChanged.to_string());
    let binding = binding.ok_or_else(changed)?;
    binding
        .current(store, credentials, providers)
        .map_err(|error| match error {
            SearchConfigError::Settings(error) => error.into(),
            _ => changed(),
        })?;
    let rules = store.policy_repo().policy_rules()?;
    match decide_action(&rules, &binding.action(arguments)) {
        Verdict::Deny { reason } => Err(HostError::Permission(reason)),
        Verdict::HumanRequired { .. } if automated => Ok(None),
        _ => Ok(Some(approved_search_pattern(
            binding.endpoint.as_str(),
            arguments,
        ))),
    }
}

fn approve_call(
    world: &mut World,
    store: &Store,
    approval: &Approval,
    call: Entity,
    arguments: &Value,
    automated: bool,
) -> Result<(Approval, Option<String>)> {
    let name = approval.tool_name.as_str();
    let repo = store.approval_repo();
    let request_id = &approval.request_id;
    let pattern = if name == web_search::NAME {
        let fallback = crate::host::providers::Providers::default();
        match search_approval_pattern(
            world.get::<SearchBinding>(call),
            store,
            world.get_resource(),
            world.get_resource().unwrap_or(&fallback),
            arguments,
            automated,
        ) {
            Ok(Some(pattern)) => Some(pattern),
            Ok(None) => {
                return Err(HostError::Permission(
                    "this search now requires user confirmation".into(),
                ));
            }
            Err(error) => {
                let status = match error {
                    HostError::Conflict(_) => ApprovalStatus::Expired,
                    HostError::Permission(_) => ApprovalStatus::Denied,
                    _ => return Err(error),
                };
                let resolved = repo.resolve_approval(request_id, status)?;
                release(world, request_id);
                refuse(world, call, &error.to_string());
                notify_resolved(&mut world.resource_mut::<Notifier>(), &resolved);
                return Err(error);
            }
        }
    } else {
        None
    };
    let binding = world.get::<CallBinding>(call);
    let rebound =
        match validate_bound_approval_client(store, approval, binding, name, arguments, automated)?
        {
            BoundClientCheck::Allowed(client) => client,
            BoundClientCheck::Denied(reason) => {
                let denied = repo.resolve_approval(request_id, ApprovalStatus::Denied)?;
                release(world, request_id);
                refuse(world, call, &reason);
                notify_resolved(&mut world.resource_mut::<Notifier>(), &denied);
                return Err(HostError::Permission(reason));
            }
            BoundClientCheck::RequiresUser => {
                return Err(HostError::Permission(
                    "this operation now requires user confirmation".into(),
                ));
            }
        };
    let resolved = repo.resolve_approval_with_client(
        request_id,
        ApprovalStatus::Approved,
        rebound.as_deref(),
    )?;
    release(world, request_id);
    if let Some(target) = rebound
        && let Some(mut binding) = world.get_mut::<CallBinding>(call)
    {
        binding.client_id = target;
    }
    world.entity_mut(call).insert(Approved);
    Ok((resolved, pattern))
}

/// Checks the approval scope, live call hash and current grants before committing the decision.
pub(crate) fn submit_with_outcome(
    world: &mut World,
    request: ApprovalRequest<'_>,
    decision: ApprovalDecision,
) -> Result<SubmitOutcome> {
    let automated = matches!(request, ApprovalRequest::DecisionModel { .. });
    let (request_id, owner) = match request {
        ApprovalRequest::Operator(id) => (id, None),
        ApprovalRequest::Companion { request, companion }
        | ApprovalRequest::DecisionModel { request, companion } => (request, Some(companion)),
    };
    let store = world.resource::<Store>().clone();
    let repo = store.approval_repo();
    let approval = repo
        .approval(request_id)?
        .ok_or_else(|| HostError::NotFound("approval not found".into()))?;
    if owner.is_some_and(|owner| owner != approval.session_id) {
        return Err(HostError::Permission(
            "this approval belongs to another companion".into(),
        ));
    }
    if !matches!(
        approval.status,
        ApprovalStatus::Pending | ApprovalStatus::Evaluating
    ) {
        return Err(HostError::Conflict("approval is no longer pending".into()));
    }
    if automated && approval.status != ApprovalStatus::Evaluating {
        return Err(HostError::Permission(
            "this operation requires user confirmation".into(),
        ));
    }
    let Some(&hold) = world.resource::<Approvals>().by_request.get(request_id) else {
        repo.resolve_approval(request_id, ApprovalStatus::Expired)?;
        return Err(HostError::Conflict(
            "approval is not attached to a live tool call".into(),
        ));
    };
    let snapshot = world.get::<ToolRequest>(hold.call).and_then(|request| {
        let name = request.call.function.name.as_str();
        let args = Value::Object(request.call.function.arguments.clone());
        (name == approval.tool_name && canonicalise(name, &args).1 == approval.action_payload_hash)
            .then_some(args)
    });
    let Some(args) = snapshot else {
        repo.resolve_approval(request_id, ApprovalStatus::Expired)?;
        release(world, request_id);
        refuse(
            world,
            hold.call,
            "the operation arguments changed after the approval was requested",
        );
        return Err(HostError::Conflict(
            "approval no longer matches the call arguments".into(),
        ));
    };

    let (approval, saved_pattern) = match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            approve_call(world, &store, &approval, hold.call, &args, automated)?
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
            let approval = repo.resolve_approval(request_id, ApprovalStatus::Denied)?;
            release(world, request_id);
            refuse(world, hold.call, "the user denied this operation");
            (approval, None)
        }
    };
    let rule = if matches!(decision, ApprovalDecision::AlwaysApprove) {
        match persist_always_allow_rule(
            &store,
            &mut world.resource_mut::<Notifier>(),
            &approval,
            saved_pattern,
        ) {
            Ok(()) => RuleSave::Saved,
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(%error, "failed to persist AlwaysApprove policy rule");
                world.resource_mut::<Notifier>().notice(
                    Some(&approval.session_id),
                    "policy_rule_save_failed",
                    &format!(
                        "承認は実行されましたが、常時許可ルールの保存に失敗しました: {message}"
                    ),
                );
                if error.is_storage() {
                    crate::host::fail(world, &message);
                }
                RuleSave::Failed(message)
            }
        }
    } else {
        RuleSave::NotRequested
    };
    notify_resolved(&mut world.resource_mut::<Notifier>(), &approval);
    Ok(SubmitOutcome { approval, rule })
}

fn notify_resolved(notifier: &mut Notifier, approval: &Approval) {
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
    args: &ResolveApprovalArgs,
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
    let approval = submit_with_outcome(
        world,
        ApprovalRequest::Companion {
            request: &args.request_id,
            companion: &companion_id,
        },
        decision,
    )?
    .approval;
    Ok(serde_json::json!({
        "request_id": approval.request_id,
        "status": approval.status.as_str(),
    })
    .to_string())
}

/// A stopped turn takes its held calls with it; the approval must not become reusable.
pub(super) fn expire_orphaned_approvals(
    mut approvals: ResMut<Approvals>,
    entities: Query<()>,
    store: Res<Store>,
    mut notifier: ResMut<Notifier>,
    mut decisions: ResMut<Decisions>,
    mut commands: Commands,
) {
    let repo = store.into_inner().approval_repo();
    approvals.by_request.retain(|request, hold| {
        if entities.contains(hold.call) {
            return true;
        }
        commands.entity(hold.hold).try_despawn();
        decisions.running.remove(request);
        match repo.resolve_approval(request, ApprovalStatus::Expired) {
            Ok(expired) => notify_resolved(&mut notifier, &expired),
            Err(ene_store::StoreError::Unavailable(_)) => return true,
            Err(error) if error.is_storage() => {
                commands.run_system_cached_with(crate::host::stop_on_failure, error.to_string());
            }
            Err(error) => tracing::debug!(%request, %error, "approval is already closed"),
        }
        false
    });
}
