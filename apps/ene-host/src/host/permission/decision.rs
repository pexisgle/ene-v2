//! Background Decision API evaluation for pending tool approvals.
use super::approval::{ApprovalRequest, SubmitOutcome, submit_with_outcome};
use super::gate::{MAX_TOOL_IDENTIFIER_BYTES, notify_approval_card};
use crate::host::{
    Notifier,
    agent::entry_text,
    background::{self, Job},
    db::Store,
    mind::Role,
    model::{self, Credentials},
    providers::Providers,
    resources::HostRuntime,
};
use bevy_ecs::prelude::*;
use ene_policy::{ApprovalDecision, MAX_POLICY_TEXT_CHARS, PolicyRule, PolicyText};
use ene_store::util::truncate;
use rig_core::{
    DynModel,
    completion::{CompletionRequest, Message as CompletionMessage},
    operation::Completion,
};
use rig_ecs::prelude::*;
use serde::Deserialize;
use std::{collections::HashMap, task::Poll, time::Duration};

/// How long the decision model may take.
const DECISION_TIMEOUT: Duration = Duration::from_secs(15);

/// The most decisions one host runs at once.
const MAX_IN_FLIGHT_DECISIONS: usize = 8;

/// The per-string bounds the decision prompt enforces
/// (docs/design/work.md: bounded inputs only).
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
#[derive(Clone)]
pub(super) struct Pending {
    pub(super) request: String,
    pub(super) companion: String,
    pub(super) target: String,
}

/// One decision in flight: what it decides, the task, and the model that runs it.
pub(super) struct InFlight {
    pending: Pending,
    task: Job<DecisionOutcome>,
    engine: String,
    policies: Vec<PolicyText>,
    rules: Vec<PolicyRule>,
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

    fn start(&mut self, flight: InFlight) {
        debug_assert!(
            !self.is_running(&flight.pending.request),
            "one decision per request"
        );
        self.running.insert(flight.pending.request.clone(), flight);
    }

    /// Takes the one finished task, with the approval it decides.
    fn take_finished(&mut self) -> Option<(InFlight, DecisionOutcome)> {
        let (request, outcome) = self.running.iter_mut().find_map(|(request, flight)| {
            let Poll::Ready(outcome) = flight.task.poll() else {
                return None;
            };
            Some((
                request.clone(),
                outcome.unwrap_or(Err(DecisionError::TaskMissingResult)),
            ))
        })?;
        Some((self.running.remove(&request)?, outcome))
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
    let rules: Vec<_> = inputs
        .rules
        .iter()
        .filter(|rule| rule.scope_label().is_none())
        .take(DECISION_LIST)
        .collect();
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
pub(super) async fn run_decision(model: DynModel<Completion>, prompt: String) -> DecisionOutcome {
    let request = CompletionRequest::new(prompt).preamble(DECISION_SYSTEM_PROMPT);
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
pub(super) fn request_context(
    store: &Store,
    conversation: Option<&Conversation>,
    run_id: &str,
) -> String {
    if !run_id.is_empty()
        && let Ok(Some(run)) = store.work_repo().run(run_id)
    {
        return truncate(&run.instruction, DECISION_REQUEST_CHARS);
    }
    conversation.map_or_default(latest_user_text)
}

/// The newest non-empty user message of a conversation.
fn latest_user_text(conversation: &Conversation) -> String {
    conversation
        .entries()
        .rev()
        .filter(|entry| matches!(entry.message, CompletionMessage::User { .. }))
        .find_map(|entry| entry_text(&entry))
        .map_or_default(|text| truncate(&text, DECISION_REQUEST_CHARS))
}

/// Starts the background decision for a fresh pending approval, or shows the card straight away.
pub(super) fn start_decision(
    world: &mut World,
    store: &Store,
    pending: &Pending,
    inputs: DecisionInputs,
) {
    let runtime = world.get_resource::<HostRuntime>().map(|r| r.0.clone());
    let credentials = world.get_resource::<Credentials>().cloned();
    let models = world.get_resource::<Models>().cloned();
    let providers = world
        .get_resource::<Providers>()
        .cloned()
        .unwrap_or_default();
    let wake = world.get_resource::<Wake>().cloned();
    let fatal = {
        let mut decisions = world.resource_mut::<Decisions>();
        let mut ctx = DecisionSpawnContext {
            store,
            runtime: runtime.as_ref(),
            credentials: credentials.as_ref(),
            models: models.as_ref(),
            providers: &providers,
            wake: wake.as_ref(),
            decisions: &mut decisions,
        };
        match prepare_decision_task(&mut ctx, pending, inputs) {
            DecisionSpawn::Spawned => None,
            DecisionSpawn::FallbackToCard => {
                push_card(store, &mut world.resource_mut::<Notifier>(), pending)
            }
        }
    };
    if let Some(error) = fatal {
        crate::host::fail(world, &error);
    }
}

struct DecisionSpawnContext<'a> {
    store: &'a Store,
    runtime: Option<&'a tokio::runtime::Handle>,
    credentials: Option<&'a Credentials>,
    models: Option<&'a Models>,
    providers: &'a Providers,
    wake: Option<&'a Wake>,
    decisions: &'a mut Decisions,
}

enum DecisionSpawn {
    Spawned,
    FallbackToCard,
}

fn prepare_decision_task(
    ctx: &mut DecisionSpawnContext<'_>,
    pending: &Pending,
    inputs: DecisionInputs,
) -> DecisionSpawn {
    let (Some(runtime), Some(credentials), Some(models)) =
        (ctx.runtime, ctx.credentials, ctx.models)
    else {
        return DecisionSpawn::FallbackToCard;
    };
    if ctx.decisions.is_running(&pending.request)
        || ctx.decisions.count() >= MAX_IN_FLIGHT_DECISIONS
    {
        return DecisionSpawn::FallbackToCard;
    }
    let Ok(Some(companion)) = ctx.store.companion_repo().companion(&pending.companion) else {
        return DecisionSpawn::FallbackToCard;
    };
    let model = match model::connect(
        ctx.store,
        credentials,
        models.0.catalog(),
        &companion,
        Role::Decision,
        ctx.providers,
    ) {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                companion = %pending.companion,
                %error,
                "no decision model answers; the card is shown"
            );
            return DecisionSpawn::FallbackToCard;
        }
    };
    let engine = model.spec().map_or_default(|spec| spec.id.clone());
    let task = background::spawn(
        runtime,
        ctx.wake,
        run_decision(model, decision_prompt(&inputs)),
    );
    ctx.decisions.start(InFlight {
        pending: pending.clone(),
        task,
        engine,
        policies: inputs.policies,
        rules: inputs.rules,
    });
    DecisionSpawn::Spawned
}

/// Lands one finished decision. Submission and card publication each check that
/// the approval is still pending before acting.
pub(super) fn collect_decisions(world: &mut World) {
    let Some((flight, outcome)) = world.resource_mut::<Decisions>().take_finished() else {
        return;
    };
    let pending = &flight.pending;
    let store = world.resource::<Store>().clone();
    if let Ok(decision) = &outcome
        && decision.allow
    {
        let repo = store.policy_repo();
        let result = (|| {
            if flight.policies != repo.policy_texts()? || flight.rules != repo.policy_rules()? {
                return Err(crate::host::error::HostError::Conflict(
                    "the user changed the policy during evaluation".into(),
                ));
            }
            submit_with_outcome(
                world,
                ApprovalRequest::DecisionModel {
                    request: &pending.request,
                    companion: &pending.companion,
                },
                ApprovalDecision::Approve,
            )
        })();
        if let Err(error) = &result
            && error.is_storage()
        {
            crate::host::fail(world, &error.to_string());
            return;
        }
        let accepted = result.is_ok();
        log_model_approval(pending, &flight.engine, &decision.reason, result);
        if accepted {
            return;
        }
    }
    if let Err(why) = outcome {
        tracing::warn!(%why, request = %pending.request, "the decision failed");
    }
    if let Some(fatal) = push_card(&store, &mut world.resource_mut::<Notifier>(), pending) {
        crate::host::fail(world, &fatal);
    }
}

fn log_model_approval(
    pending: &Pending,
    engine: &str,
    reason: &str,
    result: crate::host::db::Result<SubmitOutcome>,
) {
    match result {
        Ok(_) => tracing::info!(
            companion = %pending.companion,
            engine,
            reason,
            request = %pending.request,
            "the decision engine allowed the operation"
        ),
        Err(error) => tracing::debug!(
            %error,
            request = %pending.request,
            "the decision could not approve the operation"
        ),
    }
}

/// Promotes an unresolved evaluation and publishes its card once.
/// A persistence failure leaves no safe way to continue the held call.
pub(super) fn push_card(
    store: &Store,
    notifier: &mut Notifier,
    pending: &Pending,
) -> Option<String> {
    let approval = match store
        .approval_repo()
        .request_user_approval(&pending.request)
    {
        Ok(Some(approval)) => approval,
        Ok(None) => return None,
        Err(error) => return Some(error.to_string()),
    };
    notify_approval_card(notifier, &pending.companion, &approval, &pending.target);
    None
}
