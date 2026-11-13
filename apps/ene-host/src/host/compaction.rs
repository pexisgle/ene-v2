//! Context compaction: the idle trigger, the rolling summary and the request
//! fitting plan (docs/design/compaction.md).
//!
//! The session log is append-only and never rewritten. A summary snapshot is
//! saved as an `ene.summary` aside, generated off the ECS thread from a copy of
//! the log on an explicit user idle, and injected into the request view by the
//! lead. The view is kept inside the model window by [`plan_fit`] and
//! [`apply_fit`], which drop whole turns and clear old tool outputs, and leave
//! a request that cannot fit for the caller to refuse.
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
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};

/// The user-idle window that allows a soft compaction (doc §3.2).
const IDLE: Duration = Duration::from_secs(60);
/// The soft and hard token-usage ratios (doc §3.2).
const SOFT_RATIO: f64 = 0.70;
const HARD_RATIO: f64 = 0.85;
/// The pause after a dispatch, a cancellation or a failure, so a window that
/// is still tight is not retried on every frame.
const RETRY: Duration = Duration::from_secs(60);
/// The aside kind of a summary snapshot (doc §2.1).
const SUMMARY: &str = "ene.summary";
/// The line put where dropped history was (doc §5.2).
const NOTICE: &str = "[System Notice: 過去の一部ログを一時的に省略して対話を継続しています]";
/// The start of the injected summary block; `plan_fit` recognizes it.
const SUMMARY_HEAD: &str =
    "Earlier messages of this conversation were compacted into the summary below.";

/// The strict user idle clock from docs/design/compaction.md §3.1: only an
/// explicit user action (chat accept, approval) resets it; companion output
/// never does.
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

/// Records explicit user activity: resets the idle clock and cancels a running
/// summary so it is requeued for the next idle window. Call it from chat
/// acceptance and approval submission, never for companion output.
pub(crate) fn note_user_activity(world: &mut World) {
    if let Some(mut tracker) = world.get_resource_mut::<InactivityTracker>() {
        tracker.on_user_activity();
    }
    if let Some(mut state) = world.get_resource_mut::<CompactionState>()
        && state.running.is_some()
    {
        state.cancel_running();
        state.retry_after = Some(Instant::now() + RETRY);
    }
}

/// The rolling summary snapshot kept as the newest `ene.summary` aside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Summary {
    /// The summary text, ready to inject.
    pub(crate) text: String,
    /// The newest message the summary stands for; newer messages stay in the
    /// request view beside it.
    pub(crate) through_message_id: u64,
}

/// The newest summary saved in `conversation`'s log.
pub(crate) fn summary_of(conversation: &History) -> Option<Summary> {
    conversation
        .asides()
        .rev()
        .find(|(_, aside)| aside.kind == SUMMARY)
        .and_then(|(_, aside)| serde_json::from_value(aside.value.clone()).ok())
}

/// Injects `summary` at the head of the request view, in place of the messages
/// it stands for. Only the view changes; the session log keeps every entry.
/// The block is a system message so it never puts two user messages in a row
/// (docs/design/compaction.md §6).
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
    // Never split a tool call from its result, and never drop the message the
    // model is answering.
    let at = pairing::safe_start(history.messages(), count).min(len - 1);
    let summary = Entry::new(Message::system(render(&summary.text))).by(by.clone());
    history.splice(..at, [summary], by);
}

/// The text of an injected summary block.
fn render(text: &str) -> String {
    format!("{SUMMARY_HEAD}\n\n<summary>\n{}\n</summary>", text.trim())
}

fn is_summary_entry(entry: Option<EntryRef<'_>>) -> bool {
    let Some(entry) = entry else {
        return false;
    };
    let ours = matches!(entry.author.map(|author| &author.kind), Some(AuthorKind::Plugin(name)) if name == "ene-host");
    ours && matches!(
        entry.message,
        Message::System { content } if content.starts_with(SUMMARY_HEAD)
    )
}

/// What [`apply_fit`] changes in a request view (docs/design/compaction.md §5.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FitPlan {
    /// The id of the first message kept; `None` when no entry is dropped.
    pub(crate) keep_from: Option<u64>,
    /// Whether old oversized tool outputs are replaced by a placeholder.
    pub(crate) clear_tool_outputs: bool,
    /// The `[System Notice: ...]` line put where the dropped entries were.
    pub(crate) notice: Option<String>,
}

/// How a request is weighed: the fixed parts every call sends before its
/// messages (the system prompt and the tool schemas), and rig-memory's
/// [`Anchor`]: the usage a provider reported for the last reply, valid only
/// while `sent` — the [`RequestStart`] of the request it answered — still
/// describes the same prefix. Without a valid anchor the whole request is
/// counted heuristically, fixed parts included (docs/design/compaction.md §5.2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Weigher<'a> {
    /// The model reference the request goes to.
    pub(crate) model: &'a str,
    /// The system prompt, as one message.
    pub(crate) prompt: Option<&'a str>,
    /// The tool schemas the request offers.
    pub(crate) tools: &'a [Arc<ToolDefinition>],
    /// What the provider reported for the reply to `sent`.
    pub(crate) anchor: Option<Anchor>,
    /// The start of the request that reply answered, when the host kept it.
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

    /// The tokens of the fixed parts: the system prompt and the tool schemas,
    /// counted as the provider reads them.
    pub(crate) fn fixed(&self) -> u64 {
        let counter = HeuristicTokenCounter::default();
        let prompt = self
            .prompt
            .map_or(0, |prompt| counter.count(&Message::system(prompt)));
        let tools = self
            .tools
            .iter()
            .map(|tool| counter.count_tool(tool))
            .fold(0, usize::saturating_add);
        u64::try_from(prompt.saturating_add(tools)).unwrap_or(u64::MAX)
    }

    /// The tokens the request is expected to send: the anchored provider
    /// count while it still describes the request, the whole request
    /// otherwise.
    pub(crate) fn tokens(&self, history: &History) -> u64 {
        next_request_tokens(self.anchor.as_ref(), self.sent, &self.prepared(history))
    }

    /// The request's [`RequestStart`], to keep for the next anchor.
    pub(crate) fn start(&self, history: &History) -> RequestStart {
        self.prepared(history).start()
    }
}

/// The estimated tokens of `history` alone.
pub(crate) fn estimate(history: &History) -> u64 {
    rig_memory::estimate_tokens(history.messages())
}

/// The estimated tokens of one message, by the same counter the weigher uses.
fn message_tokens(message: &Message) -> u64 {
    u64::try_from(HeuristicTokenCounter::default().count(message)).unwrap_or(u64::MAX)
}

/// The tokens [`apply_fit`] injects in front of the kept messages: the summary
/// block when the head holds one, and the system notice a cut puts after it.
fn injected_tokens(history: &History) -> u64 {
    let head = history
        .entries()
        .next()
        .filter(|entry| is_summary_entry(Some(*entry)))
        .map_or(0, |entry| message_tokens(entry.message));
    head.saturating_add(message_tokens(&Message::system(NOTICE)))
}

/// Whether `history` fits `limit_tokens` while `output_reserve` stays free.
/// The fixed parts of the request count too, not only the messages.
pub(crate) fn fits(
    history: &History,
    weigher: &Weigher<'_>,
    limit_tokens: u64,
    output_reserve: u64,
) -> bool {
    weigher.tokens(history) <= limit_tokens.saturating_sub(output_reserve)
}

/// Plans how to keep a request within the model window: clear old oversized
/// tool outputs first, then drop the oldest complete turns, never splitting a
/// tool call from its result and never dropping the newest turn. The summary
/// and the `[System Notice: ...]` line the cut injects are reserved out of the
/// same budget, so [`fits`] holds again after [`apply_fit`]. A plan that drops
/// nothing while the view still does not fit is the documented refusal; the
/// caller checks it with [`fits`] after [`apply_fit`].
pub(crate) fn plan_fit(
    history: &History,
    weigher: &Weigher<'_>,
    limit_tokens: u64,
    output_reserve: u64,
) -> FitPlan {
    let budget = limit_tokens.saturating_sub(output_reserve);
    let tokens = weigher.tokens(history);
    if history.messages().is_empty() || tokens <= budget {
        return FitPlan::default();
    }
    let clearing = clearing();
    let cleared_tokens = clearing.plan(history).cleared.tokens as u64;
    if tokens.saturating_sub(cleared_tokens) <= budget {
        return FitPlan {
            clear_tool_outputs: cleared_tokens > 0,
            ..FitPlan::default()
        };
    }
    // Still too large after clearing: keep the newest messages within what is
    // left of the budget once the fixed parts and the injected blocks are
    // paid for. Only the shared messages are copied to measure the cleared
    // view; the conversation itself is not cloned.
    let len = history.messages().len();
    let head = usize::from(is_summary_entry(history.entries().next()));
    let mut messages = history.messages().to_vec();
    clearing.clear(&mut messages);
    let keep_budget = budget
        .saturating_sub(weigher.fixed())
        .saturating_sub(injected_tokens(history));
    let keep_budget = usize::try_from(keep_budget).unwrap_or(usize::MAX);
    let kept = view(&messages, &TokenBudget::new(keep_budget)).len();
    let mut at = pairing::safe_start(&messages, len - kept);
    // Start the window where a request starts, so no turn is cut in half, and
    // never drop the newest turn the model is answering.
    if let Some(next) = history.entries().skip(at).find(|entry| entry.is_request()) {
        at = next.index;
    }
    at = at.min(len - 1).min(last_turn(history)).max(head);
    let keep_from = (at > head)
        .then(|| history.id(at).map(|id| id.get()))
        .flatten();
    FitPlan {
        keep_from,
        clear_tool_outputs: cleared_tokens > 0,
        notice: keep_from.as_ref().map(|_| NOTICE.to_owned()),
    }
}

/// Applies a [`FitPlan`] to the request view: one recorded splice by the host,
/// with the system notice standing where the dropped entries were. Both the
/// notice and the summary block are system messages, so injection never puts
/// two user messages in a row.
pub(crate) fn apply_fit(history: &mut History, plan: &FitPlan) {
    let by = Author::plugin("ene-host");
    let head = usize::from(is_summary_entry(history.entries().next()));
    let at = plan.keep_from.and_then(|keep_from| {
        history
            .entries()
            .position(|entry| entry.id.get() == keep_from)
    });
    if let Some(at) = at
        && at > head
    {
        let with: Vec<Entry> = plan
            .notice
            .iter()
            .map(|notice| Entry::new(Message::system(notice.clone())).by(by.clone()))
            .collect();
        history.splice(head..at, with, by.clone());
    }
    if plan.clear_tool_outputs {
        clearing().clear_conversation(history, by);
    }
}

/// The clearing policy: when the window is under pressure, every old oversized
/// tool output gives way, while small outputs and error results stay.
fn clearing() -> ClearToolOutputs {
    ClearToolOutputs::new(0)
}

/// The index of the newest message that asks something: the turn the model is
/// answering, which a window may never cut.
fn last_turn(history: &History) -> usize {
    history
        .entries()
        .rev()
        .find(|entry| entry.is_request())
        .map(|entry| entry.index)
        .unwrap_or_else(|| history.messages().len().saturating_sub(1))
}

/// A summary task running off the ECS thread on the host's one background
/// runtime. Cancelling it aborts the task, so the in-flight model request is
/// dropped instead of only being ignored once it finishes.
struct RunningSummary {
    companion: String,
    task: tokio::task::JoinHandle<()>,
    result: crossbeam_channel::Receiver<std::result::Result<Summary, String>>,
}

/// The host's compaction state: at most one summary task at a time, and the
/// time the next dispatch may start.
#[derive(Resource, Default)]
pub(crate) struct CompactionState {
    running: Option<RunningSummary>,
    retry_after: Option<Instant>,
}

impl CompactionState {
    fn cancel_running(&mut self) {
        if let Some(running) = self.running.take() {
            running.task.abort();
        }
    }
}

impl Drop for CompactionState {
    fn drop(&mut self) {
        self.cancel_running();
    }
}

/// The host's compaction plugin: the idle trigger and the summary collection.
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

/// Dispatches a background summary when the documented conditions hold: the
/// soft one (ratio >= 70% and 60s of user idle) or the hard one (ratio >= 85%,
/// which keeps the conversation going while the summary runs).
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
    if state.running.is_some() || host.is_some_and(|host| host.failed.is_some() || host.closing) {
        return;
    }
    let now = Instant::now();
    if state.retry_after.is_some_and(|after| now < after) {
        return;
    }
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
            state.retry_after = Some(now + RETRY);
            return;
        };
        // The log may move on while the summary is written, so the task reads
        // a copy; it never touches the ECS world or the session log.
        let history = conversation.0.clone();
        let previous = summary_of(&history).map_or_else(String::new, |summary| summary.text);
        let spec = Arc::clone(&bound.connection.spec);
        let handler = bound.connection.handler.0.clone();
        let (sender, result) = crossbeam_channel::unbounded();
        let task_wake = wake.as_deref().cloned();
        // One background runtime serves every summary; the task is aborted on
        // cancellation, which drops the request in flight.
        let spawned = embedder.spawn_handle(async move {
            let summary = generate(history, previous, spec, handler).await;
            let _ = sender.send(summary);
            if let Some(wake) = task_wake {
                wake.wake();
            }
        });
        match spawned {
            Some(task) => {
                state.running = Some(RunningSummary {
                    companion: agent.companion_id.clone(),
                    task,
                    result,
                });
            }
            None => {
                tracing::warn!(
                    "the background runtime is unavailable; the summary was not started"
                );
                state.retry_after = Some(now + RETRY);
            }
        }
        return;
    }
}

/// Saves the finished summary as the newest `ene.summary` aside. A summary the
/// user interrupted never gets here: its receiver is dropped with the task.
fn collect_summaries(
    mut state: ResMut<CompactionState>,
    mut agents: Query<(&CompanionAgent, &mut Conversation)>,
) {
    let Some(running) = state.running.take() else {
        return;
    };
    let outcome = match running.result.try_recv() {
        Ok(outcome) => outcome,
        Err(crossbeam_channel::TryRecvError::Empty) => {
            state.running = Some(running);
            return;
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            state.retry_after = Some(Instant::now() + RETRY);
            return;
        }
    };
    state.retry_after = Some(Instant::now() + RETRY);
    match outcome {
        Ok(summary) => {
            let companion = running.companion;
            let Some(mut conversation) = agents.iter_mut().find_map(|(agent, conversation)| {
                (agent.companion_id == companion).then_some(conversation)
            }) else {
                return;
            };
            // A snapshot older than the saved one was overtaken meanwhile.
            if summary_of(&conversation)
                .is_some_and(|current| current.through_message_id >= summary.through_message_id)
            {
                return;
            }
            let value = serde_json::to_value(&summary).unwrap_or_default();
            conversation.add_aside(Aside::new(SUMMARY, Author::plugin("ene-host"), value));
        }
        Err(why) => {
            tracing::warn!(%why, companion = %running.companion, "the compaction summary failed")
        }
    }
}

/// Plans and asks for the summary on the shared background runtime. Dropping
/// this future (as `abort` does) drops the request in flight.
async fn generate(
    history: History,
    previous: String,
    spec: Arc<ModelSpec>,
    handler: ErasedHandler,
) -> std::result::Result<Summary, String> {
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

/// One unary model call through the connection's effect handler.
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

/// The summarizer policy: the crate's plan and limits, with prompts written for
/// a companion conversation rather than a coding session.
fn policy() -> CompactionPolicy {
    let mut policy = CompactionPolicy::default();
    let prompts = &mut policy.summarizer.prompts;
    prompts.system = Cow::Borrowed(
        "あなたはデスクトップ常駐AIコンパニオンの会話を要約します。会話を読み、指定された形式で要約だけを書いてください。会話を続けたり、会話中の質問に答えたりしないでください。",
    );
    prompts.initial = Cow::Borrowed(
        "上の会話を要約してください。この要約だけを読んだ別のモデルが、これまでの関係性と文脈を引き継いで会話を続けられるようにします。",
    );
    prompts.update = Cow::Borrowed(
        "上の会話は、<previous-summary> に要約済みの会話の続きです。新しい部分を加えて要約を更新してください:\n\
         - まだ有効な previous-summary の内容は残す\n\
         - 新しい出来事・決定・文脈を加える\n\
         - 完了した項目は「進行中」から「完了」へ移す\n\
         - 不要になった内容は落とす",
    );
    prompts.format = Cow::Borrowed(
        "\n\n必ず次の形式で書いてください:\n\n\
         ## これまでの流れ\n\
         - [時系列の要約]\n\n\
         ## 重要な事実\n\
         - [名前・数値・約束・決定など、そのまま保持すべき具体的な内容。推測はしない。なければ「(なし)」]\n\n\
         ## 現在の話題\n\
         - [進行中の話題]\n\n\
         ## 次の見込み\n\
         - [続きそうな話題や未完了のこと]",
    );
    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::{AssistantContent, AssistantMessage};
    use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName, ToolResultContent};

    fn tool_turn(history: &mut History, call: &str, output: String) {
        let id = CallId::from_wire(call);
        let function = ToolFunction::new(
            ToolName::new("read").unwrap(),
            serde_json::json!({ "path": "a.rs" }),
        );
        history.extend([
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                ToolCall::new(id.clone(), function),
            )])),
            Message::User {
                content: vec![UserContent::tool_result(
                    id,
                    ToolName::new("read").unwrap(),
                    vec![ToolResultContent::text(output)],
                )],
            },
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

    fn is_user(message: &Message) -> bool {
        matches!(message, Message::User { .. })
    }

    #[test]
    fn a_fit_keeps_tool_pairs_and_drops_whole_turns() {
        let mut history = conversation(8);
        history
            .validate()
            .expect("the fixture is a canonical conversation");
        let weight = Weigher::default();
        let limit = 600;
        let reserve = 50;
        assert!(!fits(&history, &weight, limit, reserve));
        let plan = plan_fit(&history, &weight, limit, reserve);
        let keep_from = plan.keep_from.expect("old turns are dropped");
        assert!(plan.notice.is_some());
        apply_fit(&mut history, &plan);
        history.validate().expect("tool calls keep their results");
        let mut entries = history.entries();
        let notice = entries.next().expect("the system notice");
        assert!(matches!(
            notice.message,
            Message::System { content } if content == NOTICE
        ));
        let first = entries.next().expect("a kept entry");
        assert_eq!(first.id.get(), keep_from);
        assert!(first.is_request(), "a window starts where a turn starts");
        assert!(fits(&history, &weight, limit, reserve));
    }

    #[test]
    fn a_fit_refuses_a_single_oversized_message() {
        let mut history = History::new();
        history.extend([
            Message::user("q".repeat(20_000)),
            Message::assistant("a short answer"),
        ]);
        let weight = Weigher::default();
        let plan = plan_fit(&history, &weight, 1_000, 100);
        assert_eq!(plan, FitPlan::default());
        apply_fit(&mut history, &plan);
        assert!(!fits(&history, &weight, 1_000, 100));
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
        let limit = 1_200;
        let reserve = 100;
        let mut history = conversation(6);
        // The messages alone fit, but the system prompt and tool schemas the
        // request also sends do not: the fixed parts share the same budget.
        assert!(estimate(&history) <= limit - reserve);
        assert!(!fits(&history, &weight, limit, reserve));
        let plan = plan_fit(&history, &weight, limit, reserve);
        assert!(plan.keep_from.is_some(), "the history is cut");
        apply_fit(&mut history, &plan);
        assert!(
            !fits(&history, &weight, limit, reserve),
            "fixed parts alone can still make the request refuse"
        );
        // A window that takes the fixed parts cuts fewer turns; the injected
        // notice and the kept messages fit inside the same budget.
        let limit = 2_000;
        let mut history = conversation(6);
        let plan = plan_fit(&history, &weight, limit, reserve);
        assert!(plan.keep_from.is_some(), "the history is still cut");
        apply_fit(&mut history, &plan);
        assert!(fits(&history, &weight, limit, reserve));
    }

    #[test]
    fn injected_blocks_are_system_and_never_two_user_messages() {
        let mut history = conversation(6);
        let through = history.entries().nth(3).unwrap().id.get();
        inject_summary(
            &mut history,
            &Summary {
                text: "これまでの流れ: テスト".to_owned(),
                through_message_id: through,
            },
        );
        history.validate().expect("the summary keeps tool pairs");
        let first = history.entries().next().expect("the summary");
        assert!(
            matches!(first.message, Message::System { content } if content.starts_with(SUMMARY_HEAD)),
            "the summary block is a system message"
        );
        let weight = Weigher::default();
        let plan = plan_fit(&history, &weight, 700, 50);
        assert!(plan.keep_from.is_some(), "the window is cut");
        apply_fit(&mut history, &plan);
        history.validate().expect("the notice keeps tool pairs");
        let messages: Vec<&Message> = history.messages().iter().map(Arc::as_ref).collect();
        for pair in messages.windows(2) {
            assert!(
                !(is_user(pair[0]) && is_user(pair[1])),
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
        let back: Summary = serde_json::from_value(value).unwrap();
        assert_eq!(back, summary);
    }

    #[test]
    fn only_user_activity_resets_the_idle_clock() {
        let mut app = App::new();
        app.add_plugins(CompactionPlugin);
        app.world_mut()
            .resource_mut::<InactivityTracker>()
            .last_user_interaction_at = Instant::now() - IDLE - Duration::from_secs(5);
        // A frame of companion-side work leaves the clock alone.
        app.update();
        assert!(app.world().resource::<InactivityTracker>().idle_duration() > IDLE);
        note_user_activity(app.world_mut());
        assert!(
            app.world().resource::<InactivityTracker>().idle_duration() < Duration::from_secs(1)
        );
    }

    /// Sets `dropped` when the task it guards is dropped, as an aborted
    /// summary drops its in-flight request.
    struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn user_activity_aborts_a_running_summary() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(std::sync::Arc::clone(&dropped));
        let task = runtime.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let mut world = World::new();
        world.insert_resource(InactivityTracker::default());
        let (_sender, result) = crossbeam_channel::unbounded();
        world.insert_resource(CompactionState {
            running: Some(RunningSummary {
                companion: "ene".to_owned(),
                task,
                result,
            }),
            retry_after: None,
        });
        note_user_activity(&mut world);
        assert!(world.resource::<CompactionState>().running.is_none());
        runtime.block_on(tokio::task::yield_now());
        assert!(
            dropped.load(std::sync::atomic::Ordering::Relaxed),
            "cancellation drops the in-flight request, not only a flag"
        );
    }
}
