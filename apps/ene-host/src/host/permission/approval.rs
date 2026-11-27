//! Approval submission, hold release, rule persistence, and orphan expiration.
use super::{
    decision::Decisions,
    gate::{Approvals, Approved, CallBinding},
    resolve::PreparedPluginTool,
};
use crate::host::{
    db::Store,
    error::{HostError, Result},
    notify::Notifier,
    web_search::{self, PreparedSearch, SearchConfigError},
};
use bevy_ecs::prelude::*;
use ene_policy::{
    Approval, ApprovalDecision, ApprovalStatus, CandidateAction, InvocationScope, PolicyEffect,
    PolicyRule, Verdict, canonicalise, decide_action, decide_capability, hex_digest,
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
    scoped: Option<PolicyRule>,
    requires_scope: bool,
) -> Result<()> {
    let rule = match scoped {
        Some(rule) => rule,
        None if requires_scope => {
            return Err(HostError::Unavailable(
                "the external approval scope is unavailable".into(),
            ));
        }
        None => PolicyRule {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name: approval.tool_name.clone(),
            pattern: if approval.normalized_arguments.trim().is_empty() {
                "*".to_owned()
            } else if approval.normalized_arguments.chars().count() <= 4_000 {
                approval.normalized_arguments.clone()
            } else {
                format!("exact_sha256:{}", hex_digest(&approval.action_payload_hash))
            },
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        },
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

fn revalidate_search(
    search: Option<&PreparedSearch>,
    store: &Store,
    credentials: Option<&crate::host::model::Credentials>,
    providers: &crate::host::providers::Providers,
) -> Result<(Verdict, Option<PolicyRule>)> {
    let changed = || HostError::Conflict(SearchConfigError::ConnectionChanged.to_string());
    let search = search.ok_or_else(changed)?;
    search
        .route
        .binding
        .current(store, credentials, providers)
        .map_err(|error| match error {
            SearchConfigError::Settings(error) => error.into(),
            _ => changed(),
        })?;
    revalidate_action(store, &search.candidate(), search.action.scope.as_ref())
}

fn revalidate_action(
    store: &Store,
    action: &CandidateAction,
    scope: Option<&InvocationScope>,
) -> Result<(Verdict, Option<PolicyRule>)> {
    let verdict = decide_action(&store.policy_repo().policy_rules()?, action);
    if let Verdict::Deny { reason } = verdict {
        return Err(HostError::Permission(reason));
    }
    Ok((verdict, scope.map(InvocationScope::allow_rule)))
}

fn approve_call(
    world: &mut World,
    store: &Store,
    approval: &Approval,
    call: Entity,
    arguments: &Value,
    automated: bool,
) -> Result<(Approval, Option<PolicyRule>)> {
    let name = approval.tool_name.as_str();
    let repo = store.approval_repo();
    let request_id = &approval.request_id;
    let verdict = if let Some(tool) = world.get::<PreparedPluginTool>(call) {
        if tool.revoked() {
            Err(HostError::Conflict(
                "the tool plugin was replaced or disabled".into(),
            ))
        } else {
            revalidate_action(store, &tool.candidate(), tool.action.scope.as_ref())
        }
    } else if name == web_search::NAME {
        let fallback = crate::host::providers::Providers::default();
        revalidate_search(
            world.get::<PreparedSearch>(call),
            store,
            world.get_resource(),
            world.get_resource().unwrap_or(&fallback),
        )
    } else {
        Ok((Verdict::Allow, None))
    };
    let scoped_rule = match verdict {
        Ok((Verdict::HumanRequired { .. }, _)) if automated => {
            return Err(HostError::Permission(
                "this operation now requires user confirmation".into(),
            ));
        }
        Ok((_, rule)) => rule,
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
    let grant = if automated {
        let policy = store.policy_repo();
        Approved::Decision {
            texts: policy.policy_texts()?,
            rules: policy.policy_rules()?,
        }
    } else {
        Approved::Human
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
    world.entity_mut(call).insert(grant);
    Ok((resolved, scoped_rule))
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
    if matches!(request, ApprovalRequest::Companion { .. })
        && approval.status != ApprovalStatus::Pending
    {
        return Err(HostError::Permission(
            "there is no user approval to answer yet".into(),
        ));
    }
    let Some(hold) = world
        .resource::<Approvals>()
        .by_request
        .get(request_id)
        .cloned()
    else {
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

    let (approval, scoped_rule) = match decision {
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
    if approval.work_id.is_some()
        && let Some(mut runs) = world.get_resource_mut::<crate::host::work::Runs>()
    {
        runs.touch(&approval.run_id);
    }
    let rule = if matches!(decision, ApprovalDecision::AlwaysApprove) {
        let requires_scope = approval.tool_name == web_search::NAME
            || world.get::<PreparedPluginTool>(hold.call).is_some();
        match persist_always_allow_rule(
            &store,
            &mut world.resource_mut::<Notifier>(),
            &approval,
            scoped_rule,
            requires_scope,
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

/// A stopped turn takes its held calls with it; the approval must not become reusable.
pub(super) fn expire_orphaned_approvals(
    mut approvals: ResMut<Approvals>,
    mut calls: Query<(Option<&PreparedPluginTool>, Option<&mut Decision>)>,
    store: Res<Store>,
    mut notifier: ResMut<Notifier>,
    mut decisions: ResMut<Decisions>,
    mut commands: Commands,
) {
    let repo = store.into_inner().approval_repo();
    approvals.by_request.retain(|request, hold| {
        if let Ok((owner, decision)) = calls.get_mut(hold.call) {
            if owner.is_none_or(|owner| !owner.revoked()) {
                return true;
            }
            if let Some(mut decision) = decision {
                decision.decide(
                    Decider::plugin("ene-host"),
                    Outcome::refused("the tool plugin was replaced or disabled"),
                );
            }
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
