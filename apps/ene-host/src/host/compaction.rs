//! Context compaction: the rolling summary at a turn boundary and the one-pass
//! request fit (docs/design/compaction.md).
use super::{
    agent::CompanionAgent,
    background::{self, AbortOnDropHandle},
    db::Store,
    mind::{
        Role,
        consolidate::{self, Commit, Services},
        embedding::Embeddings,
    },
    model::{self, Credentials},
    resources::{HostRuntime, HostState},
};
use bevy_ecs::prelude::*;
use rig_core::catalog::ModelSpec;
use rig_core::completion::{Message, ToolDefinition};
use rig_core::conversation::{
    Aside, Author, AuthorKind, Conversation as History, Entry, EntryRef, MessageId, pairing,
};
use rig_core::{DynModel, operation::Completion};
use rig_ecs::prelude::*;
use rig_memory::recent::{TokenBudget, view};
use rig_memory::{
    Anchor, ClearToolOutputs, CompactReason, CompactionPolicy, Prepared, RequestStart, Summarizer,
    SummaryState, estimate_tokens, next_request_tokens,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// The share of its model's input limit a conversation may reach before a turn
/// boundary starts a rolling summary (doc §3.1). It is the only trigger: no
/// user action starts, resets or cancels one.
const THRESHOLD_RATIO: f64 = 0.75;
/// A provider that never answers must not block every companion's summaries.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(60);
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

// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fit(
    history: &mut History,
    model: &str,
    prompt: Option<&str>,
    tools: &[Arc<ToolDefinition>],
    anchor: Option<&Anchor>,
    sent: Option<RequestStart>,
    budget: u64,
) -> bool {
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

type Outcome = std::result::Result<Collected, String>;

/// What one job produced: the summary the aside keeps, and the memories the
/// pass committed for the same window when it succeeded.
struct Collected {
    summary: Summary,
    memories: crate::host::db::Result<Commit>,
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
#[allow(clippy::too_many_arguments)]
fn summarize_at_turn_boundary(
    mut state: ResMut<CompactionState>,
    agents: Query<SummaryRows>,
    store: Option<Res<Store>>,
    credentials: Option<Res<Credentials>>,
    models: Option<Res<Models>>,
    embeddings: Option<Res<Embeddings>>,
    host: Option<Res<HostState>>,
    runtime: Option<Res<HostRuntime>>,
    wake: Option<Res<Wake>>,
    providers: Option<Res<crate::host::providers::Providers>>,
) {
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
        let Ok(Some(companion)) = store.companion(&agent.companion_id) else {
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
    let outcome = background::take(running)
        .unwrap_or_else(|| Err("the summary task ended without a result".to_owned()));
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

/// One job: the summary, then the memory pass over the window that summary
/// stands for. The summary runs first because its `through_message_id` is the
/// window's end, so a failed summary commits nothing.
async fn run(
    history: History,
    previous: Option<Summary>,
    spec: Arc<ModelSpec>,
    model: DynModel<Completion>,
    services: Services,
    companion: String,
) -> Outcome {
    let (text, after) = previous.map_or_else(
        || (String::new(), None),
        |previous| (previous.text, Some(previous.through_message_id)),
    );
    let summary = summarize(&history, text, spec, model).await?;
    let memories = consolidate::pass(
        &services,
        &companion,
        &history,
        after,
        summary.through_message_id,
    )
    .await;
    Ok(Collected { summary, memories })
}

/// Asks for the summary on the shared background runtime.
async fn summarize(
    history: &History,
    previous: String,
    spec: Arc<ModelSpec>,
    model: DynModel<Completion>,
) -> std::result::Result<Summary, String> {
    let state = SummaryState {
        summary: previous,
        tracked: Vec::new(),
    };
    let plan = policy()
        .plan_summary(history, &state, &CompactReason::Threshold, &spec)
        .map_err(|refusal| refusal.to_string())?
        .ok_or_else(|| "the conversation is too short to summarize".to_owned())?;
    let through = through_message_id(history, plan.keep_from)
        .ok_or_else(|| "there is nothing new to summarize".to_owned())?;
    let response = tokio::time::timeout(SUMMARY_TIMEOUT, model.call(plan.request))
        .await
        .map_err(|_| "the summarizer timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    let text = Summarizer::summary_text(&response).map_err(|error| error.to_string())?;
    Ok(Summary {
        text,
        through_message_id: through,
    })
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
    use crate::host::{
        json::JsonDoc,
        mind::{Assignment, Assignments, Companion, Profile, State},
    };
    use rig_core::catalog::{Catalog, Connector, ModelSpec};
    use rig_core::completion::{AssistantContent, AssistantMessage};
    use rig_core::conversation::TurnEnd;
    use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
    use rig_core::providers::registry::ProviderId;
    use rig_core::serve::{ErasedHandler, adapters::ModelAdapter};
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
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
    fn every_cut_budget_keeps_a_complete_newest_request() {
        let history = conversation(4);
        let latest = history
            .entries()
            .rev()
            .find(|entry| entry.is_request())
            .unwrap()
            .id;
        for budget in 0..=estimate_tokens(history.messages()) {
            if let Some(id) = cut(&history, budget, 0) {
                let at = history.index_of(id).unwrap();
                assert!(history.entries().nth(at).unwrap().is_request());
                assert!(at <= history.index_of(latest).unwrap());
                History::from_messages(history.messages()[at..].iter().cloned())
                    .validate()
                    .expect("every kept result still has its call");
            }
        }
        assert_eq!(cut(&history, 0, 0), Some(latest));
    }

    #[test]
    fn a_fit_keeps_tool_pairs_and_drops_whole_turns() {
        let mut history = conversation(8);
        history.validate().expect("a canonical fixture");
        assert!(estimate_tokens(history.messages()) > 550);
        assert!(fit(&mut history, "", None, &[], None, None, 550));
        assert!(estimate_tokens(history.messages()) <= 550);
        history.validate().expect("tool calls keep their results");
        assert!(noticed(&history));
        let mut entries = history.entries();
        entries.next().expect("the notice");
        assert!(entries.next().expect("a kept entry").is_request());
    }

    #[test]
    fn fitting_clears_large_tool_outputs_only_in_the_request_view() {
        let mut saved = History::new();
        saved.push(Message::user("read the file"));
        tool_turn(&mut saved, "read-1", "x".repeat(20_000));
        saved.push(Message::assistant("done"));
        let original = saved.messages().to_vec();
        let mut request = saved.view();

        assert!(fit(&mut request, "", None, &[], None, None, 500));
        assert!(estimate_tokens(request.messages()) <= 500);
        assert_eq!(request.messages().len(), saved.messages().len());
        assert_ne!(request.messages(), saved.messages());
        assert_eq!(saved.messages(), original);
        request
            .validate()
            .expect("clearing keeps tool calls paired");
    }

    #[test]
    fn a_fit_refuses_a_single_oversized_message() {
        let mut history = History::new();
        history.extend([
            Message::user("q".repeat(20_000)),
            Message::assistant("a short answer"),
        ]);
        assert!(!fit(&mut history, "", None, &[], None, None, 900));
        assert!(estimate_tokens(history.messages()) > 900);
        assert_eq!(history.messages().len(), 2);
    }

    #[test]
    fn the_fixed_request_weight_counts_against_the_window() {
        let prompt = "あなたはデスクトップ常駐AIコンパニオンです。".repeat(60);
        let tools = [tool(&"t".repeat(2_000))];
        let mut history = conversation(6);
        assert!(estimate_tokens(history.messages()) <= 1_100);
        assert!(!fit(
            &mut history,
            "test/model",
            Some(&prompt),
            &tools,
            None,
            None,
            1_100,
        ));
        assert!(noticed(&history), "the fixed parts can still refuse");
        let mut history = conversation(6);
        assert!(fit(
            &mut history,
            "test/model",
            Some(&prompt),
            &tools,
            None,
            None,
            1_900,
        ));
        assert!(
            Prepared {
                model: "test/model",
                prompt: Some(&prompt),
                tools: &tools,
                history: &history,
            }
            .whole_tokens()
                <= 1_900
        );
        assert!(noticed(&history));
    }

    #[test]
    fn persisted_summaries_resume_without_rewriting_source_messages() {
        let temp = tempfile::tempdir().unwrap();
        let store = rig_memory::DirStore::open(temp.path()).unwrap();
        let mut source = conversation(3);
        let original_messages = source.messages().to_vec();
        let original_ids: Vec<_> = source.entries().map(|entry| entry.id).collect();
        let older = Summary {
            text: "one completed turn".into(),
            through_message_id: original_ids[3].get(),
        };
        source.add_aside(Aside::new(
            SUMMARY,
            Author::plugin("ene-host"),
            serde_json::to_value(&older).unwrap(),
        ));
        rig_memory::save(&store, "main", &mut source).unwrap();
        let older_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
        let mut source = rig_memory::load(&store, "main").unwrap();
        assert_eq!(summary_of(&source), Some(older));

        let latest = Summary {
            text: "two completed turns".into(),
            through_message_id: original_ids[7].get(),
        };
        source.add_aside(Aside::new(
            SUMMARY,
            Author::plugin("ene-host"),
            serde_json::to_value(&latest).unwrap(),
        ));
        rig_memory::save(&store, "main", &mut source).unwrap();
        let latest_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
        assert!(latest_bytes.len() > older_bytes.len());
        assert!(latest_bytes.starts_with(&older_bytes));
        drop(store);

        let store = rig_memory::DirStore::open(temp.path()).unwrap();
        let source = rig_memory::load(&store, "main").unwrap();
        let resumed = summary_of(&source).expect("the newest saved summary");
        assert_eq!(resumed, latest);
        assert_eq!(source.asides().count(), 2);
        assert_eq!(source.messages(), original_messages);
        assert_eq!(
            source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
            original_ids
        );

        let mut request = source.view();
        inject_summary(&mut request, &resumed);
        assert!(matches!(
            request.entries().next().unwrap().message,
            Message::System { content } if content.contains("two completed turns")
                && !content.contains("one completed turn")
        ));
        assert_eq!(&request.messages()[1..], &original_messages[8..]);
        assert_eq!(
            request
                .entries()
                .skip(1)
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            original_ids[8..]
        );
        request
            .validate()
            .expect("the kept tool call has its result");
        assert_eq!(source.messages(), original_messages);
        assert_eq!(
            source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
            original_ids
        );

        let persisted = rig_memory::load(&store, "main").unwrap();
        assert_eq!(persisted, source);
        assert_eq!(persisted.messages(), original_messages);
        assert_eq!(
            persisted
                .entries()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            original_ids
        );
        assert_eq!(summary_of(&persisted), Some(latest));
        assert_eq!(
            rig_memory::Store::read_log(&store, "main").unwrap(),
            latest_bytes
        );
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
        assert!(fit(&mut history, "", None, &[], None, None, 650));
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

    /// Sets `dropped` when the task it guards is dropped.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn background_handles_run_on_tokio_wake_and_abort_on_drop() {
        use std::sync::atomic::AtomicUsize;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake = Wake::new({
            let wakes = wakes.clone();
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        });
        let (done, received) = tokio::sync::oneshot::channel();
        let task = background::spawn(runtime.handle(), Some(&wake), async move {
            let value = tokio::time::timeout(Duration::from_secs(1), async { 42 })
                .await
                .unwrap();
            done.send(()).unwrap();
            value
        });
        runtime.block_on(received).unwrap();
        assert!(task.is_finished());
        assert_eq!(background::take(task), Some(42));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);

        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let task = background::spawn(runtime.handle(), Some(&wake), async move {
            let _guard = guard;
            std::future::pending::<()>().await
        });
        drop(task);
        runtime.block_on(tokio::task::yield_now());
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "cancellation does not report completion"
        );
    }

    /// One companion holding `conversation`, and the resources a job reads.
    struct SummaryWorld {
        app: App,
        agent: Entity,
        companion: String,
        /// Kept alive: the state directory and the runtime the summary runs on.
        _root: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
    }

    impl SummaryWorld {
        /// A world whose model endpoint is closed, so a summary that did run
        /// never reaches the network. Only the start runs: the threshold tests
        /// read the slot it fills.
        fn new(conversation: History, context_window: u32) -> Self {
            Self::at(
                conversation,
                context_window,
                context_window,
                "http://127.0.0.1:9",
            )
        }

        /// `threshold_window` is the room the agent's own model reports, which
        /// is what the trigger measures the conversation against;
        /// `summary_window` is the room the summarizer's keep window is
        /// measured in, so a test can cut the conversation where it wants.
        fn at(
            conversation: History,
            threshold_window: u32,
            summary_window: u32,
            base: &str,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
            store
                .update_settings(
                    store.settings_revision().unwrap(),
                    &[(connection_row(base), 0)],
                )
                .unwrap();
            let companion = companion();
            store.insert_companion(&companion).unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let credentials = Credentials::new(true);
            credentials.remember("c1", "test-key".into());
            let mut app = App::new();
            app.insert_resource(store)
                .insert_resource(credentials)
                .insert_resource(models(summary_window))
                .insert_resource(Embeddings::default())
                .insert_resource(HostRuntime(runtime.handle().clone()))
                .init_resource::<CompactionState>()
                .add_systems(Update, summarize_at_turn_boundary);
            let agent = app
                .world_mut()
                .spawn((
                    CompanionAgent {
                        companion_id: companion.id.clone(),
                    },
                    Conversation(conversation),
                    tiny_model(threshold_window),
                ))
                .id();
            Self {
                app,
                agent,
                companion: companion.id,
                _root: root,
                _runtime: runtime,
            }
        }

        /// Runs the whole job, both halves, until its slot is free again: the
        /// memories are committed and the summary is either saved or left out.
        fn run(&mut self) {
            self.app
                .add_systems(Update, collect_summaries.after(summarize_at_turn_boundary));
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut free = 0;
            while std::time::Instant::now() < deadline {
                self.app.update();
                if self.app.world().resource::<CompactionState>().is_running() {
                    free = 0;
                } else {
                    free += 1;
                    if free >= 2 {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the job did not settle");
        }

        /// The share of the companion's input limit its conversation holds.
        fn ratio(&self) -> f64 {
            let world = self.app.world();
            let conversation = world.get::<Conversation>(self.agent).unwrap();
            let limit = world
                .get::<Connection>(self.agent)
                .unwrap()
                .spec
                .input_limit()
                .unwrap();
            estimate_tokens(conversation.0.messages()) as f64 / f64::from(limit)
        }

        /// Whether a summary is in this companion's slot.
        fn summarizing(&self) -> bool {
            self.app
                .world()
                .resource::<CompactionState>()
                .companions
                .get(&self.companion)
                .is_some_and(|slot| slot.running.is_some())
        }
    }

    /// The catalog the summary and the pass resolve through: the connection's
    /// model with `context_window` tokens of room, so the summarizer's keep
    /// window is measured in the fixture's own tokens.
    fn models(context_window: u32) -> Models {
        let provider = ProviderId::catalog("openrouter").unwrap();
        let mut catalog = Catalog::builtin().clone();
        catalog.insert(ModelSpec::new(provider, "gpt-4o-mini").with_context_window(context_window));
        Models(Connector::new(catalog))
    }

    /// The agent's own model: a mocked handler with `context_window` tokens of
    /// room, so the threshold needs no conversation of that size.
    fn tiny_model(context_window: u32) -> Connection {
        let provider = ProviderId::catalog("openai").unwrap();
        let spec =
            Arc::new(ModelSpec::new(provider, "test/tiny").with_context_window(context_window));
        let model: DynModel<Completion> =
            MockCompletionModel::from_turns([MockTurn::text("unused")]).into();
        let handler = rig_ecs::effects::Handler(ErasedHandler::new(
            ModelAdapter::<Completion>::new(spec.reference(), model),
        ));
        Connection { spec, handler }
    }

    /// The settings row the job connects through: an OpenAI adapter aimed at
    /// `endpoint`.
    fn connection_row(endpoint: &str) -> crate::host::settings::Connection {
        use crate::host::settings::{
            ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        };
        crate::host::settings::Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openrouter".into(),
                    endpoint: endpoint.into(),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_ids: vec!["gpt-4o-mini".into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    fn companion() -> Companion {
        let now = crate::host::util::now();
        Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: JsonDoc::empty(),
            voice: JsonDoc::empty(),
            assignments: Assignments {
                compaction: Some(Assignment {
                    connection_id: "c1".into(),
                    model_id: "gpt-4o-mini".into(),
                }),
                consolidation: Some(Assignment {
                    connection_id: "c1".into(),
                    model_id: "gpt-4o-mini".into(),
                }),
                ..Default::default()
            },
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[test]
    fn a_companion_over_the_threshold_at_a_turn_boundary_starts_a_summary() {
        let mut world = SummaryWorld::new(conversation(4), 400);
        assert!(
            world.ratio() >= THRESHOLD_RATIO,
            "the fixture is over the threshold: {}",
            world.ratio()
        );
        world.app.update();
        assert!(
            world.summarizing(),
            "the threshold with no turn in flight starts a summary"
        );
    }

    #[test]
    fn a_companion_under_the_threshold_at_a_turn_boundary_does_not_start_a_summary() {
        let mut world = SummaryWorld::new(conversation(4), 4_000);
        assert!(
            world.ratio() < THRESHOLD_RATIO,
            "the fixture is under the threshold: {}",
            world.ratio()
        );
        world.app.update();
        assert!(!world.summarizing(), "a turn boundary alone starts nothing");
    }

    #[test]
    fn a_companion_over_the_threshold_mid_turn_does_not_start_a_summary() {
        let mut world = SummaryWorld::new(conversation(4), 400);
        assert!(
            world.ratio() >= THRESHOLD_RATIO,
            "the fixture is over the threshold: {}",
            world.ratio()
        );
        let agent = world.agent;
        world.app.world_mut().spawn(TurnOf(agent));
        assert!(
            world.app.world().get::<ActiveTurn>(agent).is_some(),
            "the turn in flight is the signal the summary waits for"
        );
        world.app.update();
        assert!(
            !world.summarizing(),
            "a turn in flight is not a turn boundary"
        );
    }

    #[test]
    fn a_turn_boundary_is_tried_once_until_the_conversation_grows() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut state = CompactionState::default();
        assert!(state.may_start("ene", 7));
        state.note_attempt("ene", 7);
        let task = background::spawn(runtime.handle(), None, std::future::pending::<Outcome>());
        state.start("ene", task);

        // The call is collected without a new turn: the same boundary again.
        state.companions.get_mut("ene").unwrap().running = None;
        assert!(
            !state.may_start("ene", 7),
            "a standing boundary is not retried"
        );
        assert!(state.may_start("ene", 8), "the next turn is a new boundary");
    }

    // === One job, one range: the summary and the memories it commits ===

    /// `turns` answered turns with a tool call each, the way a real
    /// conversation has them, so a summary has something to keep and the
    /// memory pass has answered turns to read.
    fn answered(turns: usize) -> History {
        let mut history = History::new();
        for turn in 0..turns {
            history.push(Message::user(format!("question {turn} {}", "x".repeat(40))));
            history.open_turn();
            tool_turn(&mut history, &format!("call-{turn}"), "z".repeat(400));
            history.push(Message::assistant(format!(
                "answer {turn} {}",
                "y".repeat(40)
            )));
            history.end_turn(TurnEnd::Answered { answer: None });
        }
        history
    }

    fn fact() -> serde_json::Value {
        serde_json::json!([{
            "content": "A keeps the tea in the cupboard",
            "importance": 7,
            "scope": "",
            "supersedes": null,
            "cited_memory_ids": []
        }])
    }

    /// A local OpenAI-compatible endpoint that answers both halves of a job:
    /// the extraction call with `output`, every other call with `summary`. The
    /// request bodies are kept, so a test can read what each model was asked.
    struct Provider {
        base: String,
        bodies: Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn serve(summary: &'static str, output: serde_json::Value) -> Provider {
        serve_with(summary, Some(output)).await
    }

    /// `output` `None` makes the extraction call fail: the model answers with
    /// text instead of the `submit` tool call the extractor requires.
    async fn serve_with(summary: &'static str, output: Option<serde_json::Value>) -> Provider {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let body = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        break Some(String::from_utf8_lossy(&buffer[end + 4..]).to_string());
                    }
                };
                let Some(body) = body else {
                    return;
                };
                let extraction = body.contains("\"submit\"");
                seen.lock().unwrap().push(body);
                let answer = match (&output, extraction) {
                    (Some(output), true) => tool_call(output.clone()),
                    _ => text(summary),
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(answer.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Provider { base, bodies, task }
    }

    fn text(text: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-summary",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    }

    fn tool_call(output: serde_json::Value) -> String {
        serde_json::json!({
            "id": "chatcmpl-extraction",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": "submit", "arguments": output.to_string() }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    }

    /// One compaction job covers one range. The summary it writes stands for a
    /// message window, and the memories it commits come from exactly the turns
    /// in that window, stamped with the same window end; the aside is written
    /// after the commit, so a job that committed memories has its summary.
    #[test]
    fn one_job_covers_the_same_range_for_the_summary_and_the_memories() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let provider = runtime.block_on(serve(
            "既存の紅茶の話",
            serde_json::json!({ "facts": fact() }),
        ));
        let history = answered(4);
        // The trigger is over its ratio at 400 tokens of room; the summarizer
        // keeps 300 of the 1,200 the catalog reports, which cuts before the
        // newest turns.
        let mut world = SummaryWorld::at(history.clone(), 400, 1_200, &provider.base);
        assert!(
            world.ratio() >= THRESHOLD_RATIO,
            "the fixture is over the threshold: {}",
            world.ratio()
        );
        world.run();

        let conversation = world.app.world().get::<Conversation>(world.agent);
        let conversation = conversation.expect("the companion's conversation");
        let summary = summary_of(&conversation.0).expect("the summary aside was written");
        assert_eq!(summary.text, "既存の紅茶の話");

        // The turns the summary stands for, and the ones it does not.
        let window: Vec<String> =
            consolidate::pending_turns(&history, None, summary.through_message_id)
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect();
        let all: Vec<String> = consolidate::pending_turns(&history, None, u64::MAX)
            .into_iter()
            .map(|turn| turn.turn_id)
            .collect();
        let outside: Vec<&String> = all.iter().filter(|id| !window.contains(id)).collect();
        assert!(
            !window.is_empty() && !outside.is_empty(),
            "the fixture must leave turns on both sides: {window:?} of {all:?}"
        );

        let bodies = provider.bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "one job: one summary call and one extraction call"
        );
        let extraction = bodies
            .iter()
            .find(|body| body.contains("\"submit\""))
            .expect("the pass asked the extraction model");
        for id in &window {
            assert!(
                extraction.contains(&format!("[turn {id}]")),
                "turn {id} is inside the window: {extraction}"
            );
        }
        for id in outside {
            assert!(
                !extraction.contains(&format!("[turn {id}]")),
                "turn {id} is outside the window: {extraction}"
            );
        }

        let store = world.app.world().resource::<Store>();
        let (memories, _) = store.list_memories("ene", None, 50).unwrap();
        assert_eq!(memories.len(), 1, "the job committed its one fact");
        assert_eq!(memories[0].content, "A keeps the tea in the cupboard");
        assert_eq!(
            memories[0].source_context["turn_id"],
            serde_json::json!(summary.through_message_id.to_string()),
            "the memory is stamped with the same end of the window"
        );
        provider.task.abort();
    }

    /// The aside is the position the next job starts from, so a pass that
    /// commits nothing leaves it unwritten: the stored summary stays the one
    /// the range is measured from, and the next compaction covers the range
    /// again. No queue and no cursor is involved.
    #[test]
    fn a_failed_pass_leaves_the_summary_unwritten_and_the_range_pending() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // The extraction answers with text instead of the `submit` call, so the
        // pass cannot produce facts.
        let provider = runtime.block_on(serve_with("既存の紅茶の話", None));
        let mut world = SummaryWorld::at(answered(4), 400, 1_200, &provider.base);
        world.run();

        let conversation = world.app.world().get::<Conversation>(world.agent);
        assert!(
            summary_of(&conversation.expect("the companion's conversation").0).is_none(),
            "a failed pass does not advance the stored summary"
        );
        let bodies = provider.bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "the summary ran, then the pass asked the extraction model"
        );
        assert!(
            !bodies[0].contains("\"submit\""),
            "the summary call is the first one"
        );
        assert!(
            bodies[1].contains("\"submit\""),
            "the extraction call is the one that failed"
        );
        let store = world.app.world().resource::<Store>();
        let (memories, _) = store.list_memories("ene", None, 50).unwrap();
        assert!(memories.is_empty(), "a failed pass commits nothing");
        provider.task.abort();
    }
}
