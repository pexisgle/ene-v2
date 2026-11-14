//! Context compaction: the idle rolling summary and the one-pass request fit
//! (docs/design/compaction.md).
use super::{
    agent::CompanionAgent,
    db::Store,
    mind::{Role, embedding::Embedder},
    model::{self, Credentials},
    resources::HostState,
};
use bevy_ecs::prelude::*;
use rig_core::catalog::ModelSpec;
use rig_core::completion::{Message, ToolDefinition};
use rig_core::conversation::{
    Aside, Author, AuthorKind, Conversation as History, Entry, EntryRef, MessageId, pairing,
};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::serve::{Dispatch, ErasedHandler};
use rig_ecs::prelude::*;
use rig_memory::recent::{TokenBudget, view};
use rig_memory::{
    Anchor, ClearToolOutputs, CompactReason, CompactionPolicy, HeuristicTokenCounter, Prepared,
    RequestStart, Summarizer, SummaryState, TokenCounter, next_request_tokens,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const IDLE: Duration = Duration::from_secs(60);
const SOFT_RATIO: f64 = 0.70;
const HARD_RATIO: f64 = 0.85;
const RETRY: Duration = Duration::from_secs(60);
const SUMMARY: &str = "ene.summary";
const NOTICE: &str = "[System Notice: 過去の一部ログを一時的に省略して対話を継続しています]";
const SUMMARY_HEAD: &str =
    "Earlier messages of this conversation were compacted into the summary below.";

/// The strict user idle clock (doc §3.1): only an explicit user action resets
/// it; companion output never does.
#[derive(Resource, Clone, Debug)]
pub(crate) struct InactivityTracker {
    pub(crate) last_user_interaction_at: Instant,
}

impl Default for InactivityTracker {
    fn default() -> Self {
        Self {
            last_user_interaction_at: Instant::now(),
        }
    }
}

impl InactivityTracker {
    pub(crate) fn on_user_activity(&mut self) {
        self.last_user_interaction_at = Instant::now();
    }

    pub(crate) fn idle_duration(&self) -> Duration {
        self.last_user_interaction_at.elapsed()
    }
}

/// Resets the idle clock and aborts a running summary, which is requeued for
/// the next idle window. Call it from user actions, never for companion output.
pub(crate) fn note_user_activity(world: &mut World) {
    if let Some(mut tracker) = world.get_resource_mut::<InactivityTracker>() {
        tracker.on_user_activity();
    }
    if let Some(mut state) = world.get_resource_mut::<CompactionState>() {
        let cancelled = state.cancel_running();
        let at = Instant::now() + RETRY;
        for companion in cancelled {
            state.defer(&companion, at);
        }
    }
}

/// The rolling summary snapshot kept as the newest `ene.summary` aside.
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

/// How a request is weighed: the fixed parts every call sends (system prompt and
/// tool schemas), and rig-memory's [`Anchor`], valid only while `sent` — the
/// [`RequestStart`] of the request it answered — still describes the prefix
/// (doc §5.2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Weigher<'a> {
    pub(crate) model: &'a str,
    pub(crate) prompt: Option<&'a str>,
    pub(crate) tools: &'a [Arc<ToolDefinition>],
    pub(crate) anchor: Option<Anchor>,
    pub(crate) sent: Option<RequestStart>,
}

impl Weigher<'_> {
    fn prepared<'a>(&'a self, history: &'a History) -> Prepared<'a> {
        Prepared {
            model: self.model,
            prompt: self.prompt,
            tools: self.tools,
            history,
        }
    }

    /// The tokens the request is expected to send.
    fn tokens(&self, history: &History) -> u64 {
        next_request_tokens(self.anchor.as_ref(), self.sent, &self.prepared(history))
    }

    /// The heuristic estimate of the whole request, fixed parts included.
    fn whole(&self, history: &History) -> u64 {
        self.prepared(history).whole_tokens()
    }

    /// The request's [`RequestStart`], kept for the next reply's anchor.
    pub(crate) fn start(&self, history: &History) -> RequestStart {
        self.prepared(history).start()
    }
}

/// The estimated tokens of `history` alone.
pub(crate) fn estimate(history: &History) -> u64 {
    rig_memory::estimate_tokens(history.messages())
}

fn message_tokens(message: &Message) -> u64 {
    u64::try_from(HeuristicTokenCounter::default().count(message)).unwrap_or(u64::MAX)
}

/// The tokens a cut injects in front of the kept messages: the summary block
/// when the head holds one, plus the system notice.
fn injected_tokens(history: &History) -> u64 {
    let head = history
        .entries()
        .next()
        .filter(|entry| is_summary_entry(Some(*entry)))
        .map_or(0, |entry| message_tokens(entry.message));
    head.saturating_add(message_tokens(&Message::system(NOTICE)))
}

fn fits(history: &History, weigher: &Weigher<'_>, limit_tokens: u64, output_reserve: u64) -> bool {
    weigher.tokens(history) <= limit_tokens.saturating_sub(output_reserve)
}

/// Fits the request view into `limit_tokens - output_reserve`: clears old tool
/// outputs once, then drops the oldest whole turns behind a `[System Notice: ...]`
/// line when that is still not enough. Returns whether the request fits
/// afterwards; `false` means the caller must refuse it. Fixed parts and the
/// injected blocks share the same budget.
pub(crate) fn fit(
    history: &mut History,
    weigher: &Weigher<'_>,
    limit_tokens: u64,
    output_reserve: u64,
) -> bool {
    if fits(history, weigher, limit_tokens, output_reserve) {
        return true;
    }
    let budget = limit_tokens.saturating_sub(output_reserve);
    let plan = ClearToolOutputs::new(0).plan(history);
    if plan.cleared.results > 0 {
        plan.apply(history, None, Author::plugin("ene-host"));
        if fits(history, weigher, limit_tokens, output_reserve) {
            return true;
        }
    }
    let whole = weigher.whole(history);
    let head = usize::from(is_summary_entry(history.entries().next()));
    // The fixed prompt and tool weight is the whole request minus its messages.
    let fixed = whole.saturating_sub(estimate(history));
    let keep = budget
        .saturating_sub(fixed)
        .saturating_sub(injected_tokens(history));
    if let Some(at) = cut(history, keep, head).and_then(|id| history.index_of(id)) {
        let by = Author::plugin("ene-host");
        let notice = Entry::new(Message::system(NOTICE)).by(by.clone());
        history.splice(head..at, [notice], by);
    }
    fits(history, weigher, limit_tokens, output_reserve)
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
    let start = pairing::safe_start(messages, messages.len() - kept);
    let at = history
        .entries()
        .skip(start)
        .find(|entry| entry.is_request())
        .map_or(start, |entry| entry.index)
        .min(last_turn(history))
        .max(head);
    if at > head { history.id(at) } else { None }
}

/// The index of the newest message that asks something: the turn the model is
/// answering, which a window may never cut.
fn last_turn(history: &History) -> usize {
    let newest = history.entries().rev().find(|entry| entry.is_request());
    newest.map_or(history.messages().len().saturating_sub(1), |entry| {
        entry.index
    })
}

type Outcome = std::result::Result<Summary, String>;

/// A summary task on the host's shared background runtime; cancelling it aborts
/// the task, so the in-flight request is dropped.
struct RunningSummary {
    task: tokio::task::JoinHandle<()>,
    result: Arc<Mutex<Option<Outcome>>>,
}

/// One companion's summary slot: the task in flight and its own retry clock.
#[derive(Default)]
struct CompanionSummary {
    running: Option<RunningSummary>,
    retry_after: Option<Instant>,
}

/// Background summary state keyed by companion. A failure or an interruption
/// only delays the companion it belongs to, and each companion has at most one
/// summary in flight; the shared runtime still runs one summary at a time.
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

    /// Whether `companion` may start a summary at `now`: it has no task in
    /// flight and its own cooldown has elapsed.
    fn may_start(&self, companion: &str, now: Instant) -> bool {
        self.companions.get(companion).is_none_or(|state| {
            state.running.is_none() && state.retry_after.is_none_or(|after| now >= after)
        })
    }

    fn start(&mut self, companion: &str, running: RunningSummary) {
        debug_assert!(
            self.companions
                .get(companion)
                .is_none_or(|state| state.running.is_none()),
            "one summary per companion"
        );
        self.companions
            .entry(companion.to_owned())
            .or_default()
            .running = Some(running);
    }

    /// Delays this companion's next summary without touching any other.
    fn defer(&mut self, companion: &str, until: Instant) {
        self.companions
            .entry(companion.to_owned())
            .or_default()
            .retry_after = Some(until);
    }

    /// Takes the one finished task, with the companion it belongs to.
    fn take_finished(&mut self) -> Option<(String, RunningSummary)> {
        let companion = self.companions.iter().find_map(|(companion, state)| {
            state
                .running
                .as_ref()
                .filter(|running| running.task.is_finished())
                .map(|_| companion.clone())
        })?;
        let running = self
            .companions
            .get_mut(&companion)
            .and_then(|state| state.running.take())?;
        Some((companion, running))
    }

    /// Aborts every running summary and returns the companions it aborted.
    fn cancel_running(&mut self) -> Vec<String> {
        let mut cancelled = Vec::new();
        for (companion, state) in &mut self.companions {
            if let Some(running) = state.running.take() {
                running.task.abort();
                cancelled.push(companion.clone());
            }
        }
        cancelled
    }
}

impl Drop for CompactionState {
    fn drop(&mut self) {
        self.cancel_running();
    }
}

pub(crate) struct CompactionPlugin;

impl Plugin for CompactionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InactivityTracker>()
            .init_resource::<CompactionState>()
            .add_systems(
                Update,
                (summarize_when_idle, collect_summaries)
                    .chain()
                    .after(AgentLoop),
            );
    }
}

/// Dispatches a background summary on the soft condition (ratio >= 70% and 60s
/// of user idle) or the hard one (ratio >= 85%).
#[allow(clippy::too_many_arguments)]
fn summarize_when_idle(
    tracker: Res<InactivityTracker>,
    mut state: ResMut<CompactionState>,
    agents: Query<(
        &Conversation,
        &CompanionAgent,
        Option<&Connection>,
        Option<&LastUsage>,
    )>,
    store: Option<Res<Store>>,
    credentials: Option<Res<Credentials>>,
    models: Option<Res<Models>>,
    host: Option<Res<HostState>>,
    embedder: Option<Res<Embedder>>,
    wake: Option<Res<Wake>>,
) {
    if state.is_running() || host.is_some_and(|host| host.failed.is_some() || host.closing) {
        return;
    }
    let now = Instant::now();
    let (Some(store), Some(credentials), Some(models), Some(embedder)) =
        (store, credentials, models, embedder)
    else {
        return;
    };
    let idle = tracker.idle_duration();
    for (conversation, agent, connection, usage) in &agents {
        let Some(connection) = connection else {
            continue;
        };
        let Some(limit) = connection.spec.input_limit() else {
            continue;
        };
        let tokens = usage
            .and_then(LastUsage::context)
            .unwrap_or_else(|| estimate(conversation));
        let ratio = tokens as f64 / f64::from(limit);
        if ratio < HARD_RATIO && !(ratio >= SOFT_RATIO && idle >= IDLE) {
            continue;
        }
        if !state.may_start(&agent.companion_id, now) {
            continue;
        }
        let Ok(Some(companion)) = store.companion(&agent.companion_id) else {
            continue;
        };
        let Ok(bound) = model::connect(
            &store,
            &credentials,
            models.0.catalog(),
            &companion,
            Role::Compaction,
        ) else {
            tracing::warn!(companion = %agent.companion_id, "no compaction model is assigned");
            state.defer(&agent.companion_id, now + RETRY);
            continue;
        };
        let history = conversation.0.clone();
        let previous = summary_of(&history).map_or_else(String::new, |summary| summary.text);
        let spec = Arc::clone(&bound.connection.spec);
        let handler = bound.connection.handler.0.clone();
        let result = Arc::new(Mutex::new(None));
        let task_result = Arc::clone(&result);
        let task_wake = wake.as_deref().cloned();
        let Some(task) = embedder.spawn_handle(async move {
            let outcome = generate(history, previous, spec, handler).await;
            if let Ok(mut slot) = task_result.lock() {
                *slot = Some(outcome);
            }
            if let Some(wake) = task_wake {
                wake.wake();
            }
        }) else {
            tracing::warn!("the background runtime is unavailable; the summary was not started");
            state.defer(&agent.companion_id, now + RETRY);
            continue;
        };
        state.start(&agent.companion_id, RunningSummary { task, result });
        return;
    }
}

/// Saves a finished summary as the newest `ene.summary` aside.
fn collect_summaries(
    mut state: ResMut<CompactionState>,
    mut agents: Query<(&CompanionAgent, &mut Conversation)>,
) {
    let Some((companion, running)) = state.take_finished() else {
        return;
    };
    state.defer(&companion, Instant::now() + RETRY);
    let outcome = running
        .result
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| Err("the summary task was cancelled".to_owned()));
    match outcome {
        Ok(summary) => {
            let Some(mut conversation) = agents.iter_mut().find_map(|(agent, conversation)| {
                (agent.companion_id == companion).then_some(conversation)
            }) else {
                return;
            };
            if summary_of(&conversation)
                .is_some_and(|current| current.through_message_id >= summary.through_message_id)
            {
                return;
            }
            let value = serde_json::to_value(&summary).unwrap_or_default();
            conversation.add_aside(Aside::new(SUMMARY, Author::plugin("ene-host"), value));
        }
        Err(why) => {
            tracing::warn!(%why, companion = %companion, "the compaction summary failed")
        }
    }
}

/// Asks for the summary on the shared background runtime; dropping the future
/// (as `abort` does) drops the request in flight.
async fn generate(
    history: History,
    previous: String,
    spec: Arc<ModelSpec>,
    handler: ErasedHandler,
) -> Outcome {
    let state = SummaryState {
        summary: previous,
        tracked: Vec::new(),
    };
    let plan = policy()
        .plan_summary(&history, &state, &CompactReason::Threshold, &spec)
        .map_err(|refusal| refusal.to_string())?
        .ok_or_else(|| "the conversation is too short to summarize".to_owned())?;
    let through = through_message_id(&history, plan.keep_from)
        .ok_or_else(|| "there is nothing new to summarize".to_owned())?;
    let text = completion(plan.request, handler).await?;
    Ok(Summary {
        text,
        through_message_id: through,
    })
}

async fn completion(
    request: rig_core::completion::CompletionRequest,
    handler: ErasedHandler,
) -> std::result::Result<String, String> {
    let kind = EffectKind::Completion {
        request: Arc::new(request),
        stream: false,
    };
    let dispatch = Dispatch::new(EffectId::from_raw(0), false);
    match handler.handle(kind, dispatch).await.into_outcome().await {
        Ok(Answered::Completion(response)) => {
            Summarizer::summary_text(&response).map_err(|error| error.to_string())
        }
        Ok(_) => Err("the summarizer answered something other than a completion".to_owned()),
        Err(report) => Err(report.to_string()),
    }
}

/// The id of the newest message a summary ending before `keep_from` stands for.
fn through_message_id(history: &History, keep_from: Option<MessageId>) -> Option<u64> {
    let at = match keep_from {
        Some(id) => history.index_of(id)?,
        None => history.messages().len(),
    };
    history.id(at.checked_sub(1)?).map(|id| id.get())
}

/// The summarizer policy: prompts written for a companion conversation.
fn policy() -> CompactionPolicy {
    let mut policy = CompactionPolicy::default();
    let prompts = &mut policy.summarizer.prompts;
    prompts.system = "あなたはデスクトップ常駐AIコンパニオンの会話を要約します。会話を読み、指定された形式で要約だけを書いてください。会話を続けたり、会話中の質問に答えたりしないでください。".into();
    prompts.initial = "上の会話を要約してください。この要約だけを読んだ別のモデルが、これまでの関係性と文脈を引き継いで会話を続けられるようにします。".into();
    prompts.update = "上の会話は、<previous-summary> に要約済みの会話の続きです。新しい部分を加えて要約を更新してください:\n- まだ有効な previous-summary の内容は残す\n- 新しい出来事・決定・文脈を加える\n- 完了した項目は「進行中」から「完了」へ移す\n- 不要になった内容は落とす".into();
    prompts.format = "\n\n必ず次の形式で書いてください:\n\n## これまでの流れ\n- [時系列の要約]\n\n## 重要な事実\n- [名前・数値・約束・決定など、そのまま保持すべき具体的な内容。推測はしない。なければ「(なし)」]\n\n## 現在の話題\n- [進行中の話題]\n\n## 次の見込み\n- [続きそうな話題や未完了のこと]".into();
    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::{AssistantContent, AssistantMessage};
    use rig_core::message::{
        CallId, ToolCall, ToolFunction, ToolName, ToolResultContent, UserContent,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    fn tool_turn(history: &mut History, call: &str, output: String) {
        let id = CallId::from_wire(call);
        let name = ToolName::new("read").unwrap();
        let call = ToolCall::new(
            id.clone(),
            ToolFunction::new(name.clone(), serde_json::json!({ "path": "a.rs" })),
        );
        history.extend([
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            Message::tool_result(id, name, output),
        ]);
    }

    fn conversation(turns: usize) -> History {
        let mut history = History::new();
        for turn in 0..turns {
            history.push(Message::user(format!("question {turn} {}", "x".repeat(40))));
            tool_turn(&mut history, &format!("call-{turn}"), "z".repeat(400));
            history.push(Message::assistant(format!(
                "answer {turn} {}",
                "y".repeat(40)
            )));
        }
        history
    }

    fn tool(description: &str) -> Arc<ToolDefinition> {
        Arc::new(ToolDefinition::new(
            ToolName::new("read").unwrap(),
            description,
            serde_json::json!({ "type": "object", "properties": {} }),
        ))
    }

    fn noticed(history: &History) -> bool {
        history.entries().next().is_some_and(
            |entry| matches!(entry.message, Message::System { content } if content == NOTICE),
        )
    }

    fn head_text(history: &History) -> String {
        let entry = history.entries().next().expect("an injected head");
        match entry.message {
            Message::System { content } => content.clone(),
            _ => panic!("the injected head is not a system message"),
        }
    }

    #[test]
    fn a_fit_keeps_tool_pairs_and_drops_whole_turns() {
        let mut history = conversation(8);
        history.validate().expect("a canonical fixture");
        let weight = Weigher::default();
        assert!(!fits(&history, &weight, 600, 50));
        assert!(fit(&mut history, &weight, 600, 50));
        assert!(fits(&history, &weight, 600, 50));
        history.validate().expect("tool calls keep their results");
        assert!(noticed(&history));
        let mut entries = history.entries();
        entries.next().expect("the notice");
        assert!(entries.next().expect("a kept entry").is_request());
    }

    #[test]
    fn a_fit_refuses_a_single_oversized_message() {
        let mut history = History::new();
        history.extend([
            Message::user("q".repeat(20_000)),
            Message::assistant("a short answer"),
        ]);
        let weight = Weigher::default();
        assert!(!fit(&mut history, &weight, 1_000, 100));
        assert!(!fits(&history, &weight, 1_000, 100));
        assert_eq!(history.messages().len(), 2);
    }

    #[test]
    fn the_fixed_request_weight_counts_against_the_window() {
        let prompt = "あなたはデスクトップ常駐AIコンパニオンです。".repeat(60);
        let tools = [tool(&"t".repeat(2_000))];
        let weight = Weigher {
            model: "test/model",
            prompt: Some(&prompt),
            tools: &tools,
            ..Weigher::default()
        };
        let mut history = conversation(6);
        assert!(estimate(&history) <= 1_100);
        assert!(!fits(&history, &weight, 1_200, 100));
        assert!(!fit(&mut history, &weight, 1_200, 100));
        assert!(noticed(&history), "the fixed parts can still refuse");
        let mut history = conversation(6);
        assert!(fit(&mut history, &weight, 2_000, 100));
        assert!(fits(&history, &weight, 2_000, 100));
        assert!(noticed(&history));
    }

    #[test]
    fn old_tool_outputs_are_cleared_before_turns_are_dropped() {
        let mut history = History::new();
        history.push(Message::user("question"));
        tool_turn(&mut history, "call-0", "z".repeat(4_000));
        history.push(Message::assistant("answer"));
        history.push(Message::user("follow up"));
        history.push(Message::assistant("reply"));
        let weight = Weigher::default();
        assert!(!fits(&history, &weight, 900, 0));
        assert!(fit(&mut history, &weight, 900, 0));
        assert_eq!(history.messages().len(), 6, "no turn was dropped");
        let cleared = history.entries().any(|entry| {
            matches!(
                entry.message,
                Message::User { content } if content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.content.first().and_then(ToolResultContent::as_text)
                            == Some(ClearToolOutputs::PLACEHOLDER)
                ))
            )
        });
        assert!(cleared, "the old output was replaced by the placeholder");
        history.validate().expect("tool calls keep their results");
    }

    #[test]
    fn injected_blocks_are_system_and_never_two_user_messages() {
        let mut history = conversation(6);
        let through = history.entries().nth(3).unwrap().id.get();
        let summary = Summary {
            text: "これまでの流れ: テスト".to_owned(),
            through_message_id: through,
        };
        inject_summary(&mut history, &summary);
        history.validate().expect("the summary keeps tool pairs");
        assert!(head_text(&history).starts_with(SUMMARY_HEAD));
        let weight = Weigher::default();
        assert!(fit(&mut history, &weight, 700, 50));
        history.validate().expect("the notice keeps tool pairs");
        assert!(
            head_text(&history).starts_with(SUMMARY_HEAD),
            "the summary head is kept"
        );
        let messages: Vec<&Message> = history.messages().iter().map(Arc::as_ref).collect();
        for pair in messages.windows(2) {
            assert!(
                !(matches!(pair[0], Message::User { .. })
                    && matches!(pair[1], Message::User { .. })),
                "injection never puts two user messages in a row"
            );
        }
        let notices = history
            .entries()
            .filter(
                |entry| matches!(entry.message, Message::System { content } if content == NOTICE),
            )
            .count();
        assert_eq!(notices, 1, "the notice is a system message");
    }

    #[test]
    fn a_summary_snapshot_round_trips() {
        let summary = Summary {
            text: "これまでの流れ: テスト".to_owned(),
            through_message_id: 42,
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            value.get("through_message_id").and_then(|id| id.as_u64()),
            Some(42)
        );
        assert_eq!(serde_json::from_value::<Summary>(value).unwrap(), summary);
    }

    #[test]
    fn only_user_activity_resets_the_idle_clock() {
        let mut app = App::new();
        app.add_plugins(CompactionPlugin);
        app.world_mut()
            .resource_mut::<InactivityTracker>()
            .last_user_interaction_at = Instant::now() - IDLE - Duration::from_secs(5);
        app.update();
        assert!(app.world().resource::<InactivityTracker>().idle_duration() > IDLE);
        note_user_activity(app.world_mut());
        assert!(
            app.world().resource::<InactivityTracker>().idle_duration() < Duration::from_secs(1)
        );
    }

    /// Sets `dropped` when the task it guards is dropped.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_failure_delays_only_its_own_companion() {
        let now = Instant::now();
        let mut state = CompactionState::default();
        state.defer("ene", now + RETRY);
        assert!(!state.may_start("ene", now));
        assert!(state.may_start("kuro", now));
        assert!(state.may_start("ene", now + RETRY));
    }

    #[test]
    fn a_companion_keeps_at_most_one_summary_in_flight() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let task = runtime.spawn(std::future::pending::<()>());
        let mut state = CompactionState::default();
        state.start(
            "ene",
            RunningSummary {
                task,
                result: Arc::new(Mutex::new(None)),
            },
        );
        let now = Instant::now();
        assert!(state.is_running(), "the shared runtime has one slot");
        assert!(!state.may_start("ene", now), "one summary per companion");
        assert!(
            state.may_start("kuro", now),
            "the slot is per companion; dispatch still checks the one runtime slot"
        );
    }

    #[test]
    fn user_activity_aborts_only_the_running_companions_summary() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let task = runtime.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let mut state = CompactionState::default();
        state.start(
            "ene",
            RunningSummary {
                task,
                result: Arc::new(Mutex::new(None)),
            },
        );
        let kuro_retry = Instant::now() + RETRY;
        state.defer("kuro", kuro_retry);
        let mut world = World::new();
        world.insert_resource(InactivityTracker::default());
        world.insert_resource(state);

        note_user_activity(&mut world);
        let state = world.resource::<CompactionState>();
        assert!(state.companions["ene"].running.is_none());
        assert!(state.companions["ene"].retry_after.is_some());
        assert_eq!(
            state.companions["kuro"].retry_after,
            Some(kuro_retry),
            "another companion's cooldown is untouched"
        );
        runtime.block_on(tokio::task::yield_now());
        assert!(
            dropped.load(Ordering::Relaxed),
            "cancellation drops the in-flight request, not only a flag"
        );
    }
}
