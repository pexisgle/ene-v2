//! Context compaction: the rolling summary at a turn boundary and the one-pass
//! request fit (docs/design/compaction.md).
use super::{
    agent::CompanionAgent,
    background::{self, AbortOnDropHandle},
    db::Store,
    mind::{Role, consolidate::Services, embedding::Embeddings},
    model::{self, Credentials},
    resources::{HostRuntime, HostState},
};
use bevy_ecs::prelude::*;
use job::{CompactionError, Outcome, run};
use rig_core::completion::{Message, ToolDefinition};
use rig_core::conversation::{
    Aside, Author, AuthorKind, Conversation as History, Entry, EntryRef, MessageId, pairing,
};
use rig_ecs::prelude::*;
use rig_memory::recent::{TokenBudget, view};
use rig_memory::{
    Anchor, ClearToolOutputs, Prepared, RequestStart, estimate_tokens, next_request_tokens,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

// Only the tests reach these through `use super::*`.
#[cfg(test)]
use super::mind::consolidate;
#[cfg(test)]
use rig_core::{DynModel, operation::Completion};
#[cfg(test)]
use std::time::Duration;

mod job;

/// The share of its model's input limit a conversation may reach before a turn
/// boundary starts a rolling summary (doc §3.1). It is the only trigger: no
/// user action starts, resets or cancels one.
const THRESHOLD_RATIO: f64 = 0.75;
const SUMMARY: &str = "ene.summary";
const NOTICE: &str = "[System Notice: 過去の一部ログを一時的に省略して対話を継続しています]";
const SUMMARY_HEAD: &str =
    "Earlier messages of this conversation were compacted into the summary below.";

/// The rolling summary snapshot kept as the newest `ene.summary` aside.
///
/// The snapshot is view-only by design (docs/design/compaction.md §2.1): the
/// append-only session log is never rewritten, so rig-memory's `apply_summary`
/// (which splices older messages out in place) and the `summary_state` /
/// `summary_in` bookkeeping of that splice are not the host's path. The
/// summary text is injected into the request view alone, by
/// [`inject_summary`], and the aside makes it survive a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub(crate) text: String,
    /// The newest message the summary stands for.
    pub(crate) through_message_id: u64,
}

pub(crate) fn summary_of(conversation: &History) -> Option<Summary> {
    conversation
        .asides()
        .rev()
        .find(|(_, aside)| aside.kind == SUMMARY)
        .and_then(|(_, aside)| serde_json::from_value(aside.value.clone()).ok())
}

/// Injects `summary` at the head of the request view, in place of the messages
/// it stands for. It is a system message, so it never puts two user messages in
/// a row (doc §6).
pub(crate) fn inject_summary(history: &mut History, summary: &Summary) {
    let len = history.messages().len();
    if len == 0 || is_summary_entry(history.entries().next()) {
        return;
    }
    let by = Author::plugin("ene-host");
    let count = history
        .entries()
        .take_while(|entry| entry.id.get() <= summary.through_message_id)
        .count();
    let at = pairing::safe_start(history.messages(), count).min(len - 1);
    let text = format!(
        "{SUMMARY_HEAD}\n\n<summary>\n{}\n</summary>",
        summary.text.trim()
    );
    history.splice(..at, [Entry::new(Message::system(text)).by(by.clone())], by);
}

fn is_summary_entry(entry: Option<EntryRef<'_>>) -> bool {
    entry.is_some_and(|entry| {
        matches!(entry.author.map(|author| &author.kind), Some(AuthorKind::Plugin(name)) if name == "ene-host")
            && matches!(entry.message, Message::System { content } if content.starts_with(SUMMARY_HEAD))
    })
}

/// The tokens a cut injects in front of the kept messages: the summary block
/// when the head holds one, plus the system notice.
fn injected_tokens(history: &History) -> u64 {
    let head = history
        .entries()
        .next()
        .filter(|entry| is_summary_entry(Some(*entry)))
        .map_or(0, |entry| estimate_tokens(&[entry.message]));
    head.saturating_add(estimate_tokens(&[Message::system(NOTICE)]))
}

pub(crate) struct FitRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) prompt: Option<&'a str>,
    pub(crate) tools: &'a [Arc<ToolDefinition>],
    pub(crate) anchor: Option<&'a Anchor>,
    pub(crate) sent: Option<RequestStart>,
    pub(crate) budget: u64,
}

pub(crate) fn fit(history: &mut History, request: &FitRequest<'_>) -> bool {
    let FitRequest {
        model,
        prompt,
        tools,
        anchor,
        sent,
        budget,
    } = *request;
    let tokens = |history: &History| {
        next_request_tokens(
            anchor,
            sent,
            &Prepared {
                model,
                prompt,
                tools,
                history,
            },
        )
    };
    if tokens(history) <= budget {
        return true;
    }
    let cleared = ClearToolOutputs::new(0).clear_conversation(history, Author::plugin("ene-host"));
    if cleared.results > 0 && tokens(history) <= budget {
        return true;
    }
    let whole = Prepared {
        model,
        prompt,
        tools,
        history,
    }
    .whole_tokens();
    let head = usize::from(is_summary_entry(history.entries().next()));
    // The fixed prompt and tool weight is the whole request minus its messages.
    let fixed = whole.saturating_sub(estimate_tokens(history.messages()));
    let keep = budget
        .saturating_sub(fixed)
        .saturating_sub(injected_tokens(history));
    if let Some(at) = cut(history, keep, head).and_then(|id| history.index_of(id)) {
        let by = Author::plugin("ene-host");
        let notice = Entry::new(Message::system(NOTICE)).by(by.clone());
        history.splice(head..at, [notice], by);
    }
    tokens(history) <= budget
}

/// The id of the first message of the newest window whose messages fit
/// `budget`: a request boundary, never before `head`, never after the newest
/// request.
fn cut(history: &History, budget: u64, head: usize) -> Option<MessageId> {
    let messages = history.messages();
    let kept = view(
        messages,
        &TokenBudget::new(usize::try_from(budget).unwrap_or(usize::MAX)),
    )
    .len();
    let start = messages.len() - kept;
    let at = history
        .entries()
        .skip(start)
        .find(rig_core::conversation::EntryRef::is_request)
        .map_or(start, |entry| entry.index)
        .min(last_turn(history))
        .max(head);
    if at > head { history.id(at) } else { None }
}

/// The index of the newest message that asks something: the turn the model is
/// answering, which a window may never cut.
fn last_turn(history: &History) -> usize {
    let newest = history
        .entries()
        .rev()
        .find(rig_core::conversation::EntryRef::is_request);
    newest.map_or(history.messages().len().saturating_sub(1), |entry| {
        entry.index
    })
}

/// One companion's summary slot: the call in flight and the newest message the
/// last attempt was made for.
#[derive(Default)]
struct CompanionSummary {
    running: Option<AbortOnDropHandle<Outcome>>,
    tried_at: Option<u64>,
}

/// Background summary state keyed by companion: at most one summary per
/// companion and one on the shared runtime at a time. Each turn boundary is
/// tried once, so a failed summary waits for the next turn instead of retrying
/// on a clock, and a published one cannot restart itself on the stale context
/// figure the provider reported before it.
#[derive(Resource, Default)]
pub(crate) struct CompactionState {
    companions: HashMap<String, CompanionSummary>,
}

impl CompactionState {
    /// The runtime-wide slot: at most one summary task runs at a time.
    fn is_running(&self) -> bool {
        self.companions
            .values()
            .any(|state| state.running.is_some())
    }

    /// Whether `companion` is due at a boundary whose newest message is
    /// `newest`: it has no task in flight, and it has not tried at this
    /// boundary already.
    fn may_start(&self, companion: &str, newest: u64) -> bool {
        self.companions.get(companion).is_none_or(|state| {
            state.running.is_none() && state.tried_at.is_none_or(|tried| newest > tried)
        })
    }

    /// Spends this boundary, so the next attempt waits for the conversation to
    /// grow: a failure is retried at the next turn, never on the same one.
    fn note_attempt(&mut self, companion: &str, newest: u64) {
        self.companions
            .entry(companion.to_owned())
            .or_default()
            .tried_at = Some(newest);
    }

    fn start(&mut self, companion: &str, running: AbortOnDropHandle<Outcome>) {
        let slot = self.companions.entry(companion.to_owned()).or_default();
        debug_assert!(slot.running.is_none(), "one summary per companion");
        slot.running = Some(running);
    }

    /// Takes the one finished task, with the companion it belongs to.
    fn take_finished(&mut self) -> Option<(String, AbortOnDropHandle<Outcome>)> {
        let companion = self.companions.iter().find_map(|(companion, state)| {
            state
                .running
                .as_ref()
                .filter(|running| running.is_finished())
                .map(|_| companion.clone())
        })?;
        let running = self
            .companions
            .get_mut(&companion)
            .and_then(|state| state.running.take())?;
        Some((companion, running))
    }
}

pub(crate) struct CompactionPlugin;

impl Plugin for CompactionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CompactionState>().add_systems(
            Update,
            (summarize_at_turn_boundary, collect_summaries)
                .chain()
                .after(AgentLoop),
        );
    }
}

/// The agent rows a summary start reads: its conversation, the model it is
/// connected to and what that model last reported, and whether it has a turn in
/// flight.
type SummaryRows = (
    &'static Conversation,
    &'static CompanionAgent,
    Option<&'static Connection>,
    Option<&'static LastUsage>,
    Has<ActiveTurn>,
);

/// Starts one background summary for a companion over [`THRESHOLD_RATIO`] once
/// it has no turn in flight, which is the turn boundary (doc §3.1).
#[derive(bevy_ecs::system::SystemParam)]
struct BoundaryServices<'w> {
    store: Option<Res<'w, Store>>,
    credentials: Option<Res<'w, Credentials>>,
    models: Option<Res<'w, Models>>,
    embeddings: Option<Res<'w, Embeddings>>,
    host: Option<Res<'w, HostState>>,
    runtime: Option<Res<'w, HostRuntime>>,
    wake: Option<Res<'w, Wake>>,
    providers: Option<Res<'w, crate::host::providers::Providers>>,
}

fn summarize_at_turn_boundary(
    mut state: ResMut<CompactionState>,
    agents: Query<SummaryRows>,
    resources: BoundaryServices<'_>,
) {
    let BoundaryServices {
        store,
        credentials,
        models,
        embeddings,
        host,
        runtime,
        wake,
        providers,
    } = resources;
    if state.is_running() || host.is_some_and(|host| host.failed.is_some() || host.closing) {
        return;
    }
    let (Some(store), Some(credentials), Some(models), Some(embeddings), Some(runtime)) =
        (store, credentials, models, embeddings, runtime)
    else {
        return;
    };
    // The pass shares the compaction job's services, so it resolves the
    // companion's own `Consolidation` model and recall's vector cache.
    let services = Services {
        store: (*store).clone(),
        embeddings: (*embeddings).clone(),
        credentials: Some((*credentials).clone()),
        models: (*models).clone(),
        providers: providers.as_deref().cloned().unwrap_or_default(),
    };
    for (conversation, agent, connection, usage, busy) in &agents {
        // A turn in flight is not a boundary; the next one is.
        if busy {
            continue;
        }
        let Some(connection) = connection else {
            continue;
        };
        let Some(limit) = connection.spec.input_limit() else {
            continue;
        };
        let tokens = usage
            .and_then(LastUsage::context)
            .unwrap_or_else(|| estimate_tokens(conversation.messages()));
        let ratio = tokens as f64 / f64::from(limit);
        let newest = conversation
            .entries()
            .next_back()
            .map_or(0, |entry| entry.id.get());
        if ratio < THRESHOLD_RATIO || !state.may_start(&agent.companion_id, newest) {
            continue;
        }
        state.note_attempt(&agent.companion_id, newest);
        let Ok(Some(companion)) = store.companion_repo().companion(&agent.companion_id) else {
            continue;
        };
        let Ok(bound) = model::connect(
            &store,
            &credentials,
            models.0.catalog(),
            &companion,
            Role::Compaction,
            &services.providers,
        ) else {
            tracing::warn!(companion = %agent.companion_id, "no compaction model is assigned");
            continue;
        };
        // One snapshot serves both halves of the job: the summary reads it, and
        // the pass reads the same messages the summary stands for.
        let history = conversation.0.clone();
        let previous = summary_of(&history);
        let Some(spec) = bound.spec().cloned() else {
            continue;
        };
        let task = background::spawn(
            &runtime.0,
            wake.as_deref(),
            run(
                history,
                previous,
                Arc::new(spec),
                bound,
                services.clone(),
                agent.companion_id.clone(),
            ),
        );
        state.start(&agent.companion_id, task);
        return;
    }
}

/// Saves a finished summary as the newest `ene.summary` aside. The aside is the
/// position the next job starts from, so it is written only once the memories,
/// edges and reflection of the same window committed (docs/design/mind.md
/// §5.2): a failed pass leaves the stored summary alone, and the range it could
/// not consolidate waits for the next compaction instead of a queue or a cursor.
fn collect_summaries(
    mut state: ResMut<CompactionState>,
    mut agents: Query<(&CompanionAgent, &mut Conversation)>,
) {
    let Some((companion, running)) = state.take_finished() else {
        return;
    };
    let outcome = background::take(running).unwrap_or(Err(CompactionError::TaskMissingResult));
    match outcome {
        Ok(collected) => {
            if let Err(error) = collected.memories {
                tracing::warn!(
                    companion = %companion,
                    %error,
                    "the consolidation pass failed; its range stays pending"
                );
                return;
            }
            let Some(mut conversation) = agents.iter_mut().find_map(|(agent, conversation)| {
                (agent.companion_id == companion).then_some(conversation)
            }) else {
                return;
            };
            if summary_of(&conversation).is_some_and(|current| {
                current.through_message_id >= collected.summary.through_message_id
            }) {
                return;
            }
            let value = serde_json::to_value(&collected.summary).unwrap_or_default();
            conversation.add_aside(Aside::new(SUMMARY, Author::plugin("ene-host"), value));
        }
        Err(why) => {
            tracing::warn!(%why, companion = %companion, "the compaction summary failed")
        }
    }
}

#[cfg(test)]
mod tests;
