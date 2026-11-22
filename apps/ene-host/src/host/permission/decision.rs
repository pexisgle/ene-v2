//! Background Decision API evaluation for pending tool approvals.
use super::approval::submit;
use super::gate::{MAX_TOOL_IDENTIFIER_BYTES, notify_approval_card};
use crate::host::{
    agent::entry_text,
    background::{self, AbortOnDropHandle},
    db::Store,
    mind::Role,
    model::{self, Credentials},
    providers::Providers,
    resources::HostRuntime,
    util::truncate,
};
use bevy_ecs::prelude::*;
use ene_policy::{ApprovalDecision, ApprovalStatus, MAX_POLICY_TEXT_CHARS, PolicyRule, PolicyText};
use rig_core::{
    DynModel,
    completion::{CompletionRequest, Message as CompletionMessage},
    operation::Completion,
};
use rig_ecs::prelude::*;
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};

/// How long the decision model may take.
const DECISION_TIMEOUT: Duration = Duration::from_secs(15);

/// The most decisions one host runs at once.
const MAX_IN_FLIGHT_DECISIONS: usize = 8;

/// The per-string bounds the decision prompt enforces
/// (docs/design/permissions.md §3.1: bounded inputs only).
pub(super) const DECISION_ARGUMENT_CHARS: usize = 2_000;
pub(super) const DECISION_REQUEST_CHARS: usize = 1_000;
const DECISION_RULE_CHARS: usize = 200;
const DECISION_REASON_CHARS: usize = 300;
/// The most policies and rules each prompt lists.
const DECISION_LIST: usize = 32;

/// The bounded inputs one decision reads.
pub(super) struct DecisionInputs {
    pub(super) tool_name: String,
    pub(super) target: String,
    pub(super) arguments: String,
    pub(super) request: String,
    pub(super) policies: Vec<PolicyText>,
    pub(super) rules: Vec<PolicyRule>,
}

/// One decision the gate asked for: the approval it decides and the card
/// context it fails safe to.
pub(super) struct Pending {
    pub(super) request: String,
    pub(super) companion: String,
    pub(super) target: String,
}

/// One decision in flight: what it decides, the task, and the model that runs it.
pub(super) struct InFlight {
    pending: Pending,
    task: AbortOnDropHandle<DecisionOutcome>,
    engine: String,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DecisionError {
    #[error("the decision model timed out")]
    Timeout,
    #[error("{0}")]
    Model(String),
    #[error("the decision reply was not the strict JSON asked for")]
    InvalidJson,
    #[error("the decision reply named an unknown verdict")]
    UnknownVerdict,
    #[error("the decision task ended without a result")]
    TaskMissingResult,
}

pub(super) type DecisionOutcome = std::result::Result<ModelDecision, DecisionError>;

/// One answer from the decision engine, with the short reason it gave.
pub(super) struct ModelDecision {
    pub(super) allow: bool,
    pub(super) reason: String,
}

/// Background decision state keyed by request id.
#[derive(Resource, Default)]
pub(crate) struct Decisions {
    pub(super) running: HashMap<String, InFlight>,
}

impl Decisions {
    fn is_running(&self, request: &str) -> bool {
        self.running.contains_key(request)
    }

    fn count(&self) -> usize {
        self.running.len()
    }

    fn start(
        &mut self,
        pending: Pending,
        task: AbortOnDropHandle<DecisionOutcome>,
        engine: String,
    ) {
        debug_assert!(
            !self.is_running(&pending.request),
            "one decision per request"
        );
        self.running.insert(
            pending.request.clone(),
            InFlight {
                pending,
                task,
                engine,
            },
        );
    }

    /// Takes the one finished task, with the approval it decides.
    fn take_finished(&mut self) -> Option<InFlight> {
        let request = self
            .running
            .iter()
            .find_map(|(request, flight)| flight.task.is_finished().then(|| request.clone()))?;
        self.running.remove(&request)
    }
}

/// The decision engine's instruction.
const DECISION_SYSTEM_PROMPT: &str = "あなたはデスクトップ常駐AIコンパニオンの権限判定エンジンです。機械的な許可ルールを通らず保留中の端末操作を、依頼文脈・ユーザーの自由文ポリシー・保存済みルールに照らして判定します。操作が安全で依頼とポリシーに適合するなら allow、危険・ポリシー抵触・依頼文脈と合わない・判断に迷うなら require_approval としてください。判定だけを次のJSON形式で返し、それ以外の文章は一切書かないでください:\n{\"decision\": \"allow\" | \"require_approval\", \"reason\": \"一行の短い理由\"}";

/// The one question the decision model gets. Every string is bounded, every
/// list capped, so the prompt is the whole input surface.
pub(super) fn decision_prompt(inputs: &DecisionInputs) -> String {
    let mut prompt = format!(
        "[依頼]\n{}\n\n[操作]\nツール: {}\n対象: {}\n引数: {}",
        or_none(&inputs.request),
        truncate(&inputs.tool_name, MAX_TOOL_IDENTIFIER_BYTES),
        truncate(&inputs.target, DECISION_ARGUMENT_CHARS),
        truncate(&inputs.arguments, DECISION_ARGUMENT_CHARS),
    );
    prompt.push_str("\n\n[ユーザーの自由文ポリシー]");
    let policies: Vec<_> = inputs.policies.iter().take(DECISION_LIST).collect();
    if policies.is_empty() {
        prompt.push_str("\n(なし)");
    } else {
        for policy in policies {
            prompt.push_str(&format!(
                "\n- {}",
                truncate(&policy.text, MAX_POLICY_TEXT_CHARS)
            ));
        }
    }
    prompt.push_str("\n\n[保存済みルール（参考）]");
    let rules: Vec<_> = inputs.rules.iter().take(DECISION_LIST).collect();
    if rules.is_empty() {
        prompt.push_str("\n(なし)");
    } else {
        for rule in rules {
            prompt.push_str(&format!(
                "\n- {} / {} → {}",
                truncate(&rule.tool_name, MAX_TOOL_IDENTIFIER_BYTES),
                truncate(&rule.pattern, DECISION_RULE_CHARS),
                rule.effect.as_str(),
            ));
        }
    }
    prompt
}

fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(なし)"
    } else {
        text
    }
}

/// The decision engine's reply: strict JSON, both fields required, nothing else accepted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionReply {
    decision: String,
    reason: String,
}

/// Asks the decision model one question and parses its strict JSON reply.
pub(super) async fn run_decision(
    model: DynModel<Completion>,
    inputs: DecisionInputs,
) -> DecisionOutcome {
    let request = CompletionRequest::new(decision_prompt(&inputs)).preamble(DECISION_SYSTEM_PROMPT);
    let response = tokio::time::timeout(DECISION_TIMEOUT, model.call(request))
        .await
        .map_err(|_| DecisionError::Timeout)?
        .map_err(|error| DecisionError::Model(error.to_string()))?;
    let reply: DecisionReply =
        serde_json::from_str(response.text().trim()).map_err(|_| DecisionError::InvalidJson)?;
    let allow = match reply.decision.as_str() {
        "allow" => true,
        "require_approval" => false,
        _ => return Err(DecisionError::UnknownVerdict),
    };
    Ok(ModelDecision {
        allow,
        reason: truncate(reply.reason.trim(), DECISION_REASON_CHARS),
    })
}

/// The user's request the decision judges the operation against: the run's
/// instruction for a hand, else the companion's newest user message.
pub(super) fn request_context(world: &World, store: &Store, agent: Entity, run_id: &str) -> String {
    if !run_id.is_empty()
        && let Ok(Some(run)) = store.work_repo().run(run_id)
    {
        return truncate(&run.instruction, DECISION_REQUEST_CHARS);
    }
    world
        .get::<Conversation>(agent)
        .map(latest_user_text)
        .unwrap_or_default()
}

/// The newest non-empty user message of a conversation.
fn latest_user_text(conversation: &Conversation) -> String {
    conversation
        .entries()
        .rev()
        .filter(|entry| matches!(entry.message, CompletionMessage::User { .. }))
        .find_map(|entry| entry_text(&entry))
        .map(|text| truncate(&text, DECISION_REQUEST_CHARS))
        .unwrap_or_default()
}

/// Starts the background decision for a fresh pending approval, or shows the card straight away.
pub(super) fn start_decision(
    world: &mut World,
    store: &Store,
    pending: Pending,
    inputs: DecisionInputs,
) {
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|r| r.0.clone()) else {
        push_card(world, &pending);
        return;
    };
    let (Some(credentials), Some(models)) = (
        world.get_resource::<Credentials>().cloned(),
        world.get_resource::<Models>(),
    ) else {
        push_card(world, &pending);
        return;
    };
    let busy = {
        let decisions = world.resource::<Decisions>();
        decisions.is_running(&pending.request) || decisions.count() >= MAX_IN_FLIGHT_DECISIONS
    };
    if busy {
        push_card(world, &pending);
        return;
    }
    let Ok(Some(companion)) = store.companion_repo().companion(&pending.companion) else {
        push_card(world, &pending);
        return;
    };
    let providers = world
        .get_resource::<Providers>()
        .cloned()
        .unwrap_or_default();
    let model = match model::connect(
        store,
        &credentials,
        models.0.catalog(),
        &companion,
        Role::Decision,
        &providers,
    ) {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                companion = %pending.companion,
                %error,
                "no decision model answers; the card is shown"
            );
            push_card(world, &pending);
            return;
        }
    };
    let engine = model.spec().map(|spec| spec.id.clone()).unwrap_or_default();
    let wake = world.get_resource::<Wake>().cloned();
    let task = background::spawn(&runtime, wake.as_ref(), run_decision(model, inputs));
    world
        .resource_mut::<Decisions>()
        .start(pending, task, engine);
}

fn handle_decision_read_error(
    world: &mut World,
    pending: &Pending,
    error: &crate::host::error::HostError,
) {
    if error.is_storage() {
        crate::host::fail(world, error.to_string());
    } else {
        tracing::warn!(%error, request = %pending.request, "decision collection failed to read approval");
        push_card(world, pending);
    }
}

fn is_decision_approval_pending(world: &mut World, pending: &Pending) -> bool {
    let store = world.resource::<Store>().clone();
    let request = &pending.request;
    let approval = match store.approval_repo().approval(request) {
        Ok(Some(approval)) => approval,
        Ok(None) => {
            tracing::debug!(%request, "a late decision dropped: no such approval");
            return false;
        }
        Err(error) => {
            handle_decision_read_error(world, pending, &error.into());
            return false;
        }
    };
    if approval.status != ApprovalStatus::Pending {
        tracing::debug!(
            %request,
            "a late decision dropped: the approval was already answered"
        );
        return false;
    }
    true
}

/// Lands one finished decision.
pub(super) fn collect_decisions(world: &mut World) {
    let Some(flight) = world.resource_mut::<Decisions>().take_finished() else {
        return;
    };
    if !is_decision_approval_pending(world, &flight.pending) {
        return;
    }
    let outcome = background::take(flight.task).unwrap_or(Err(DecisionError::TaskMissingResult));
    apply_decision_outcome(world, &flight.pending, &flight.engine, outcome);
}

fn apply_decision_outcome(
    world: &mut World,
    pending: &Pending,
    engine: &str,
    outcome: DecisionOutcome,
) {
    let request = &pending.request;
    match outcome {
        Ok(decision) if decision.allow => approve_from_decision(world, pending, engine, &decision),
        Ok(decision) => require_approval_from_decision(world, pending, engine, &decision),
        Err(why) => {
            tracing::warn!(%why, %request, "the decision failed; the card is shown");
            push_card(world, pending);
        }
    }
}

fn approve_from_decision(
    world: &mut World,
    pending: &Pending,
    engine: &str,
    decision: &ModelDecision,
) {
    let request = &pending.request;
    match submit(world, request, ApprovalDecision::Approve) {
        Ok(_) => tracing::info!(
            companion = %pending.companion,
            %engine,
            reason = %decision.reason,
            %request,
            "the decision engine allowed the operation"
        ),
        Err(error) => tracing::debug!(
            %error,
            %request,
            "a late decision dropped: the approval was already answered"
        ),
    }
}

fn require_approval_from_decision(
    world: &mut World,
    pending: &Pending,
    engine: &str,
    decision: &ModelDecision,
) {
    tracing::info!(
        companion = %pending.companion,
        %engine,
        reason = %decision.reason,
        request = %pending.request,
        "the decision engine requires the user's approval"
    );
    push_card(world, pending);
}

/// The approval card and its notice, pushed only if still pending.
pub(super) fn push_card(world: &mut World, pending: &Pending) {
    let store = world.resource::<Store>().clone();
    let approval = match store.approval_repo().approval(&pending.request) {
        Ok(Some(approval)) => approval,
        Ok(None) => return,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            } else {
                tracing::warn!(%error, request = %pending.request, "failed to read approval for card");
            }
            return;
        }
    };
    if approval.status != ApprovalStatus::Pending {
        return;
    }
    notify_approval_card(world, &pending.companion, &approval, &pending.target);
}
