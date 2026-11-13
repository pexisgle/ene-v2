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
    mind::Role,
    model::{self, Credentials},
    resources::HostState,
};
use bevy_ecs::prelude::*;
use rig_core::catalog::ModelSpec;
use rig_core::completion::Message;
use rig_core::conversation::{
    Aside, Author, AuthorKind, Conversation as History, Entry, EntryRef, MessageId, pairing,
};
use rig_core::effect::{Answered, EffectId, EffectKind};
use rig_core::message::UserContent;
use rig_core::serve::{Dispatch, ErasedHandler};
use rig_ecs::prelude::*;
use rig_memory::recent::{TokenBudget, view};
use rig_memory::{ClearToolOutputs, CompactReason, CompactionPolicy, Summarizer, SummaryState};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    let summary = Entry::new(Message::user(render(&summary.text))).by(by.clone());
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
        Message::User { content }
            if matches!(content.first(), Some(UserContent::Text(text)) if text.text.starts_with(SUMMARY_HEAD))
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

/// The estimated tokens of `history`.
pub(crate) fn estimate(history: &History) -> u64 {
    rig_memory::estimate_tokens(history.messages())
}

/// Whether `history` fits `limit_tokens` while `output_reserve` stays free.
pub(crate) fn fits(history: &History, limit_tokens: u64, output_reserve: u64) -> bool {
    estimate(history) <= limit_tokens.saturating_sub(output_reserve)
}

/// Plans how to keep a request within the model window: clear old oversized
/// tool outputs first, then drop the oldest complete turns, never splitting a
/// tool call from its result and never dropping the newest turn. A plan that
/// drops nothing while the view still does not fit is the documented refusal;
/// the caller checks it with [`fits`] after [`apply_fit`].
pub(crate) fn plan_fit(history: &History, limit_tokens: u64, output_reserve: u64) -> FitPlan {
    let budget = limit_tokens.saturating_sub(output_reserve);
    let tokens = estimate(history);
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
    // Still too large after clearing: keep the newest messages within the
    // budget and start the window where a turn starts, so no turn is cut in
    // half. A copy is cleared to measure what the view will hold.
    let mut cleared = history.clone();
    clearing.clear_conversation(&mut cleared, Author::plugin("ene-host"));
    let budget = usize::try_from(budget).unwrap_or(usize::MAX);
    let kept = view(cleared.messages(), &TokenBudget::new(budget)).len();
    let len = history.messages().len();
    let head = usize::from(is_summary_entry(history.entries().next()));
    let mut at = pairing::safe_start(history.messages(), len - kept);
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
/// with the system notice standing where the dropped entries were.
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
            .map(|notice| Entry::new(Message::user(notice.clone())).by(by.clone()))
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

/// A summary task running off the ECS thread.
struct RunningSummary {
    companion: String,
    cancel: Arc<AtomicBool>,
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
            running.cancel.store(true, Ordering::Relaxed);
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
    wake: Option<Res<Wake>>,
) {
    if state.running.is_some() || host.is_some_and(|host| host.failed.is_some() || host.closing) {
        return;
    }
    let now = Instant::now();
    if state.retry_after.is_some_and(|after| now < after) {
        return;
    }
    let (Some(store), Some(credentials), Some(models)) = (store, credentials, models) else {
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
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let thread_wake = wake.as_deref().cloned();
        let spawned = std::thread::Builder::new()
            .name("ene-compaction".into())
            .spawn(move || {
                let summary = generate(history, previous, spec, handler, &thread_cancel);
                if !thread_cancel.load(Ordering::Relaxed) {
                    let _ = sender.send(summary);
                    if let Some(wake) = thread_wake {
                        wake.wake();
                    }
                }
            });
        match spawned {
            Ok(_) => {
                state.running = Some(RunningSummary {
                    companion: agent.companion_id.clone(),
                    cancel,
                    result,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "the compaction task did not start");
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

/// Plans and asks for the summary on the compaction thread.
fn generate(
    history: History,
    previous: String,
    spec: Arc<ModelSpec>,
    handler: ErasedHandler,
    cancel: &AtomicBool,
) -> std::result::Result<Summary, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".to_owned());
    }
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("compaction runtime: {error}"))?;
    let text = runtime.block_on(completion(plan.request, handler))?;
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

    #[test]
    fn a_fit_keeps_tool_pairs_and_drops_whole_turns() {
        let mut history = History::new();
        for turn in 0..8 {
            history.push(Message::user(format!("question {turn} {}", "x".repeat(40))));
            tool_turn(&mut history, &format!("call-{turn}"), "z".repeat(400));
            history.push(Message::assistant(format!(
                "answer {turn} {}",
                "y".repeat(40)
            )));
        }
        history
            .validate()
            .expect("the fixture is a canonical conversation");
        let limit = 600;
        let reserve = 50;
        assert!(!fits(&history, limit, reserve));
        let plan = plan_fit(&history, limit, reserve);
        let keep_from = plan.keep_from.expect("old turns are dropped");
        assert!(plan.notice.is_some());
        apply_fit(&mut history, &plan);
        history.validate().expect("tool calls keep their results");
        let mut entries = history.entries();
        let notice = entries.next().expect("the system notice");
        assert!(matches!(
            notice.message,
            Message::User { content }
                if matches!(content.first(), Some(UserContent::Text(text)) if text.text == NOTICE)
        ));
        let first = entries.next().expect("a kept entry");
        assert_eq!(first.id.get(), keep_from);
        assert!(first.is_request(), "a window starts where a turn starts");
        assert!(fits(&history, limit, reserve));
    }

    #[test]
    fn a_fit_refuses_a_single_oversized_message() {
        let mut history = History::new();
        history.extend([
            Message::user("q".repeat(20_000)),
            Message::assistant("a short answer"),
        ]);
        let plan = plan_fit(&history, 1_000, 100);
        assert_eq!(plan, FitPlan::default());
        apply_fit(&mut history, &plan);
        assert!(!fits(&history, 1_000, 100));
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

    #[test]
    fn user_activity_cancels_a_running_summary() {
        let mut world = World::new();
        world.insert_resource(InactivityTracker::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_sender, result) = crossbeam_channel::unbounded();
        world.insert_resource(CompactionState {
            running: Some(RunningSummary {
                companion: "ene".to_owned(),
                cancel: Arc::clone(&cancel),
                result,
            }),
            retry_after: None,
        });
        note_user_activity(&mut world);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(world.resource::<CompactionState>().running.is_none());
    }
}
