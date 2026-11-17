//! Incremental consolidation: answered turns are read after the stored
//! cursor, a background model extracts structured facts, the host validates
//! citations and commits memories, edges and the new cursor in one SQLite
//! transaction (docs/design/mind.md §5.2).
//!
//! Only the model call runs on the background task owned by `mind::embedding`;
//! its result comes back to the ECS thread and is committed there.
use super::{
    Companion, Role,
    embedding::{self, Embeddings},
};
use crate::host::{
    agent::CompanionAgent,
    db::{Result, Store},
    error::HostError,
    model::{self, Credentials},
    notify::Notifier,
};
use bevy_ecs::prelude::*;
use rig_agent::extractor::ExtractorBuilder;
use rig_core::completion::Message;
use rig_core::conversation::{Conversation as CoreConversation, TurnEnd};
use rig_core::{DynModel, operation::Completion};
use rig_ecs::model::Models;
use rig_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// How many answered turns one job consumes.
const MAX_TURNS: usize = 20;
/// Unprocessed turns that force a job without waiting for idle.
const TURN_THRESHOLD: usize = 5;
/// How long the conversation must be quiet before an idle job runs.
const IDLE_WINDOW: Duration = Duration::from_secs(60);
/// Failed jobs wait this long before the next settle or idle retries them.
const RETRY_DELAY: Duration = Duration::from_secs(30);
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_FACTS: usize = 32;
const MAX_CONTEXT: usize = 20;
const MAX_CITATIONS: usize = 64;
/// The longest turn text used as the retrieval query for merge candidates.
const MAX_QUERY_CHARS: usize = 2_000;
/// How many newest turns are joined into that query.
const QUERY_TURNS: usize = 4;

/// One answered turn as data: the id the host advances the cursor to and the
/// text the extraction model reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) turn_id: String,
    pub(crate) text: String,
}

/// An existing memory the extraction model may merge into or supersede. The
/// revision is the CAS expectation captured before the model ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryContext {
    pub(crate) id: String,
    pub(crate) revision: u64,
    pub(crate) importance: u8,
    pub(crate) content: String,
}

/// Everything one commit needs, captured when the job started. The turns are
/// already consumed by the prompt.
#[derive(Clone, Debug)]
pub(crate) struct Batch {
    pub(crate) companion_id: String,
    /// The cursor read at job start; `None` when no cursor row exists yet.
    pub(crate) job_start: Option<String>,
    /// The cursor this commit advances to.
    pub(crate) new_cursor: String,
    pub(crate) context: Vec<MemoryContext>,
    /// Memory ids the host actually injected into those turns.
    pub(crate) injected: Vec<String>,
}

/// One extracted fact. `audience` is `""` (private), `"*"` (every
/// companion) or another companion id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(inline, deny_unknown_fields)]
pub(crate) struct Fact {
    #[schemars(length(max = 4000))]
    pub(crate) content: String,
    #[serde(default)]
    #[schemars(range(min = 0, max = 10))]
    pub(crate) importance: Option<u8>,
    #[serde(default)]
    #[schemars(length(max = 128))]
    pub(crate) audience: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 128))]
    pub(crate) supersedes: Option<String>,
    #[serde(default)]
    #[schemars(inner(length(max = 128)))]
    pub(crate) cited_memory_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Extraction {
    facts: Vec<Fact>,
}

/// The result of one committed job.
pub(crate) struct Commit {
    pub(crate) inserted: usize,
    pub(crate) updated: usize,
    pub(crate) cursor: String,
}

/// The answered turns after `cursor`, in conversation order.
pub(crate) fn pending_turns(
    conversation: &CoreConversation,
    cursor: Option<&str>,
    limit: usize,
) -> Vec<Turn> {
    if limit == 0 {
        return Vec::new();
    }
    let cursor = match cursor {
        Some(cursor) => match cursor.parse::<u64>() {
            Ok(cursor) => Some(cursor),
            // An unreadable cursor must not make the host replay everything.
            Err(_) => return Vec::new(),
        },
        None => None,
    };
    let records: Vec<_> = conversation.turns().collect();
    let mut turns = Vec::new();
    for (position, (_, record)) in records.iter().enumerate() {
        if !matches!(record.end, Some(TurnEnd::Answered { .. })) {
            continue;
        }
        let start = record.start.get();
        if cursor.is_some_and(|cursor| start <= cursor) {
            continue;
        }
        let from = conversation.start_of(record.start);
        let to = records
            .get(position + 1)
            .map_or(conversation.entries().len(), |(_, next)| {
                conversation.start_of(next.start)
            });
        let mut text = String::new();
        for entry in conversation
            .entries()
            .skip(from)
            .take(to.saturating_sub(from))
        {
            if entry.is_stop_marker() {
                continue;
            }
            let Some(line) = crate::host::agent::entry_text(&entry) else {
                continue;
            };
            let speaker = if entry.is_users_own() {
                "ユーザー"
            } else if matches!(entry.message, Message::Assistant(_)) {
                "コンパニオン"
            } else {
                continue;
            };
            text.push_str(speaker);
            text.push_str(": ");
            text.push_str(&line);
            text.push('\n');
        }
        if text.trim().is_empty() {
            continue;
        }
        turns.push(Turn {
            turn_id: start.to_string(),
            text,
        });
        if turns.len() >= limit {
            break;
        }
    }
    turns
}

/// Host-side citation guard: a cited id counts only when the host really
/// injected it into the turn being consolidated.
pub(crate) fn validate_citations(cited: &[String], injected: &[String]) -> Vec<String> {
    let injected: HashSet<&str> = injected.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    cited
        .iter()
        .filter(|id| injected.contains(id.as_str()) && seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// Resolves the companion's `Consolidation` assignment (or its documented
/// companion fallback) to a connected model.
fn target(
    store: &Store,
    credentials: Option<&Credentials>,
    models: &Models,
    companion: &Companion,
    providers: &crate::host::providers::Providers,
) -> Option<DynModel<Completion>> {
    let credentials = credentials?;
    model::connect(
        store,
        credentials,
        models.0.catalog(),
        companion,
        Role::Consolidation,
        providers,
    )
    .ok()
}

/// The extraction system prompt: facts only, separate records, never revive
/// something the user asked to forget.
const SYSTEM_PROMPT: &str = "あなたはコンパニオンの経験記憶を整理する抽出器です。\
会話から長期記憶として残す価値のある事実だけを抽出します。\
推測・願望・ロールプレイ上の虚構を事実として保存してはいけません。\
既存記憶と同一の事実は新規作成せず、supersedes に既存IDを指定します。\
importance 1以下（忘却済み）の既存記憶を高重要度で復活させてはいけません。\
cited_memory_ids には、提示された「注入済み記憶ID」に含まれるIDだけを書きます。\
出力は指定されたJSONスキーマのみです。";

/// Renders the extraction prompt. It carries the transcript, the candidate
/// memories with their revisions, and the ids the host injected.
pub(crate) fn prompt(turns: &[Turn], context: &[MemoryContext], injected: &[String]) -> String {
    let mut prompt = String::from("以下から長期記憶にする事実を抽出してください。\n\n");
    prompt.push_str("既存記憶（id / revision / importance / 内容）:\n");
    if context.is_empty() {
        prompt.push_str("- なし\n");
    }
    for memory in context {
        prompt.push_str(&format!(
            "- id={} revision={} importance={} content={}\n",
            memory.id, memory.revision, memory.importance, memory.content
        ));
    }
    prompt.push_str("\n注入済み記憶ID（cited_memory_ids に書けるのはこれだけ）:\n");
    if injected.is_empty() {
        prompt.push_str("- なし\n");
    }
    for id in injected {
        prompt.push_str(&format!("- {id}\n"));
    }
    prompt.push_str("\n会話（確定済みターン）:\n");
    for turn in turns {
        prompt.push_str(&format!("[turn {}]\n{}", turn.turn_id, turn.text));
    }
    prompt.push_str(
        "\n各 fact は内容ごとに分割してください。audience は非公開なら空文字、\
共有なら \"*\"、特定の相手ならそのIDです。",
    );
    prompt
}

fn normalize(extraction: Extraction) -> Vec<Fact> {
    let mut facts = Vec::new();
    for mut fact in extraction.facts {
        fact.content = fact.content.trim().to_owned();
        if fact.content.is_empty() || fact.content.chars().count() > 4_000 {
            continue;
        }
        fact.importance = Some(fact.importance.unwrap_or(3).min(10));
        fact.audience = fact
            .audience
            .map(|audience| audience.trim().chars().take(128).collect())
            .filter(|audience: &String| !audience.is_empty());
        fact.supersedes = fact
            .supersedes
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        fact.cited_memory_ids = fact
            .cited_memory_ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .take(MAX_CITATIONS)
            .collect();
        facts.push(fact);
        if facts.len() >= MAX_FACTS {
            break;
        }
    }
    facts
}

/// The provider must submit typed facts. Text and fenced JSON are not accepted.
async fn extract(model: &DynModel<Completion>, prompt: String) -> Result<Vec<Fact>> {
    let options = model
        .spec()
        .map(|spec| spec.default_options(None))
        .unwrap_or_default();
    let extraction = ExtractorBuilder::<Extraction>::new(model.clone())
        .append_preamble(SYSTEM_PROMPT)
        .retries(0)
        .build()
        .extract(prompt)
        .options(options)
        .await
        .map_err(|error| HostError::Unavailable(format!("consolidation output: {error}")))?;
    Ok(normalize(extraction.output))
}

/// The retrieval query for a job's merge candidates: the newest turns' text,
/// bounded so a long transcript cannot become an unbounded FTS expression.
fn turn_query(turns: &[Turn]) -> String {
    let from = turns.len().saturating_sub(QUERY_TURNS);
    let mut query = String::new();
    for turn in turns.get(from..).unwrap_or_default() {
        query.push_str(&turn.text);
        query.push('\n');
    }
    crate::host::db::truncate(&query, MAX_QUERY_CHARS)
}

/// What the background extraction task posts back to the ECS thread.
///
/// The batch is the one the task filled in, and it is present only when the
/// task reached its result. A task that stopped early has no commit input,
/// only the companion whose slot it held.
struct Reply {
    companion: String,
    batch: Option<Batch>,
    result: std::result::Result<Vec<Fact>, String>,
}

/// The one report a background job makes. A job that panics, or is dropped
/// before it reports, still sends a failure: the companion's slot is released
/// and the retry delay applies, instead of staying claimed forever.
struct Report {
    sender: crossbeam_channel::Sender<Reply>,
    companion: String,
    sent: bool,
}

impl Report {
    /// Reports the batch the task filled in, together with its result. The
    /// caller moves the batch here, so the commit reads the same value the
    /// extraction wrote its context into.
    fn send(mut self, batch: Batch, result: std::result::Result<Vec<Fact>, String>) {
        self.sent = true;
        let _ = self.sender.send(Reply {
            companion: self.companion.clone(),
            batch: Some(batch),
            result,
        });
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.sender.send(Reply {
                companion: self.companion.clone(),
                batch: None,
                result: Err("the consolidation task stopped before it reported".into()),
            });
        }
    }
}

/// The single-flight guard, retry schedule, idle clock and result channel.
#[derive(Resource)]
pub(crate) struct Consolidation {
    running: HashSet<String>,
    retry_at: HashMap<String, Instant>,
    settled_at: HashMap<String, Instant>,
    scanned_at: Option<Instant>,
    sender: crossbeam_channel::Sender<Reply>,
    receiver: crossbeam_channel::Receiver<Reply>,
}

impl Default for Consolidation {
    fn default() -> Self {
        // Unbounded on purpose: the sender runs on a Tokio worker, and a full
        // bounded channel would block that worker. Single-flight per companion
        // bounds the outstanding replies, not this channel's capacity.
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            running: HashSet::new(),
            retry_at: HashMap::new(),
            settled_at: HashMap::new(),
            scanned_at: None,
            sender,
            receiver,
        }
    }
}

impl Consolidation {
    /// Claims the companion's only slot. `false` means a job is still running
    /// or a failure is in its retry delay.
    fn begin(&mut self, companion: &str, now: Instant) -> bool {
        if self.running.contains(companion)
            || self
                .retry_at
                .get(companion)
                .is_some_and(|retry| *retry > now)
        {
            return false;
        }
        self.running.insert(companion.to_owned());
        true
    }

    fn finish(&mut self, companion: &str) {
        self.running.remove(companion);
    }

    fn fail(&mut self, companion: &str, now: Instant) {
        self.running.remove(companion);
        self.retry_at
            .insert(companion.to_owned(), now + RETRY_DELAY);
    }

    fn succeed(&mut self, companion: &str) {
        self.running.remove(companion);
        self.retry_at.remove(companion);
    }

    fn sender(&self) -> crossbeam_channel::Sender<Reply> {
        self.sender.clone()
    }
}

/// Registers the guard and the two background systems.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<Consolidation>().add_systems(
        Update,
        (schedule_consolidation, drain_consolidation)
            .chain()
            .after(AgentLoop),
    );
}

/// The sources one job reads; bundled so `start` stays a normal function.
struct Sources<'a> {
    store: &'a Store,
    runtime: Option<&'a tokio::runtime::Handle>,
    embeddings: &'a Embeddings,
    credentials: Option<&'a Credentials>,
    models: &'a Models,
    providers: &'a crate::host::providers::Providers,
}

#[allow(clippy::too_many_arguments)]
fn schedule_consolidation(
    settled: Query<(&TurnOutcome, &ForAgent), Added<Settled>>,
    agents: Query<(&CompanionAgent, &Conversation)>,
    store: Res<Store>,
    runtime: Option<Res<crate::host::resources::HostRuntime>>,
    embeddings: Res<Embeddings>,
    credentials: Option<Res<Credentials>>,
    models: Res<Models>,
    providers: Res<crate::host::providers::Providers>,
    mut jobs: ResMut<Consolidation>,
) {
    let now = Instant::now();
    let mut sources = Sources {
        store: &store,
        runtime: runtime.as_deref().map(|rt| &rt.0),
        embeddings: &embeddings,
        credentials: credentials.as_deref(),
        models: &models,
        providers: &providers,
    };
    let mut answered: HashSet<String> = HashSet::new();
    for (outcome, of) in &settled {
        let TurnOutcome::Answered(_) = outcome else {
            continue;
        };
        let Ok((agent, conversation)) = agents.get(of.0) else {
            continue;
        };
        let companion = agent.companion_id.clone();
        jobs.settled_at.insert(companion.clone(), now);
        answered.insert(companion.clone());
        let Ok(cursor) = store.consolidation_cursor(&companion) else {
            continue;
        };
        let pending = pending_turns(conversation, cursor.as_deref(), MAX_TURNS);
        if pending.len() >= TURN_THRESHOLD {
            start(&mut sources, &mut jobs, &companion, cursor, pending, now);
        }
    }
    if jobs
        .scanned_at
        .is_some_and(|last| now.duration_since(last) < SCAN_INTERVAL)
    {
        return;
    }
    jobs.scanned_at = Some(now);
    for (agent, conversation) in &agents {
        let companion = &agent.companion_id;
        if answered.contains(companion) || jobs.running.contains(companion) {
            continue;
        }
        let idle = jobs
            .settled_at
            .get(companion)
            .is_none_or(|settled| now.duration_since(*settled) >= IDLE_WINDOW);
        if !idle {
            continue;
        }
        let Ok(cursor) = store.consolidation_cursor(companion) else {
            continue;
        };
        let pending = pending_turns(conversation, cursor.as_deref(), MAX_TURNS);
        if pending.is_empty() {
            continue;
        }
        start(&mut sources, &mut jobs, companion, cursor, pending, now);
    }
}

fn start(
    sources: &mut Sources<'_>,
    jobs: &mut Consolidation,
    companion_id: &str,
    job_start: Option<String>,
    pending: Vec<Turn>,
    now: Instant,
) {
    let Some(new_cursor) = pending.last().map(|turn| turn.turn_id.clone()) else {
        return;
    };
    if !jobs.begin(companion_id, now) {
        return;
    }
    let companion = match sources.store.companion(companion_id) {
        Ok(Some(companion)) => companion,
        Ok(None) => {
            jobs.finish(companion_id);
            return;
        }
        Err(_) => {
            jobs.fail(companion_id, now);
            return;
        }
    };
    let Some(target) = target(
        sources.store,
        sources.credentials,
        sources.models,
        &companion,
        sources.providers,
    ) else {
        jobs.fail(companion_id, now);
        return;
    };
    // Merge candidates come from the same hybrid retrieval recall uses: FTS5
    // over the newest turns plus vector similarity, with pinned and important
    // memories only as a fallback. Forgotten memories surface when the topic
    // matches, so the model sees that the user asked to forget them.
    let query = turn_query(&pending);
    let injected =
        match sources
            .store
            .injected_between(companion_id, job_start.as_deref(), &new_cursor)
        {
            Ok(ids) => ids,
            Err(_) => {
                jobs.fail(companion_id, now);
                return;
            }
        };
    let mut batch = Batch {
        companion_id: companion_id.to_owned(),
        job_start,
        new_cursor,
        context: Vec::new(),
        injected,
    };
    let sender = jobs.sender();
    // A world with no host runtime cannot run the extraction; the job fails
    // the same way an explicit defer would, so the next turn retries it.
    let Some(runtime) = sources.runtime else {
        jobs.fail(companion_id, now);
        return;
    };
    let store = sources.store.clone();
    let embeddings = sources.embeddings.clone();
    let credentials = sources.credentials.cloned();
    let report = Report {
        sender,
        companion: companion_id.to_owned(),
        sent: false,
    };
    runtime.spawn(async move {
        let result = async {
            let semantic = embedding::semantic_ids(
                &store,
                &embeddings,
                credentials.as_ref(),
                &batch.companion_id,
                &query,
            )
            .await;
            batch.context =
                store.consolidation_context(&batch.companion_id, &query, &semantic, MAX_CONTEXT)?;
            extract(&target, prompt(&pending, &batch.context, &batch.injected)).await
        }
        .await
        .map_err(|error: HostError| error.to_string());
        report.send(batch, result);
    });
}

fn drain_consolidation(
    store: Res<Store>,
    mut jobs: ResMut<Consolidation>,
    mut notifier: Option<ResMut<Notifier>>,
) {
    while let Ok(reply) = jobs.receiver.try_recv() {
        let Reply {
            companion,
            batch,
            result,
        } = reply;
        let message = match (batch, result) {
            (Some(batch), Ok(facts)) => match store.consolidate(&batch, &facts) {
                Ok(commit) => {
                    jobs.succeed(&companion);
                    tracing::debug!(
                        companion = %companion,
                        inserted = commit.inserted,
                        updated = commit.updated,
                        cursor = %commit.cursor,
                        "consolidation committed"
                    );
                    continue;
                }
                Err(error) => error.to_string(),
            },
            (_, Err(error)) => error,
            // The sender only reports facts with the batch it filled in.
            (None, Ok(_)) => ("the consolidation task reported facts without its batch").into(),
        };
        jobs.fail(&companion, Instant::now());
        if let Some(notifier) = notifier.as_mut() {
            notifier.notice(
                Some(&companion),
                "consolidation_failed",
                &crate::host::db::truncate(&message, 300),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::memories::MemoryRecord;
    use rig_core::conversation::MessageId;
    use std::sync::Arc;

    fn answer(conversation: &mut CoreConversation, user: &str, reply: &str) -> MessageId {
        let start = conversation.push(Message::user(user));
        conversation.open_turn();
        conversation.push(Message::assistant(reply));
        conversation.end_turn(TurnEnd::Answered { answer: None });
        start
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn batch(cursor: Option<&str>, next: &str) -> Batch {
        Batch {
            companion_id: "ene".into(),
            job_start: cursor.map(ToOwned::to_owned),
            new_cursor: next.into(),
            context: Vec::new(),
            injected: Vec::new(),
        }
    }

    /// A background job that panics or is dropped before it reports must not
    /// leave the companion claimed forever.
    #[test]
    fn a_job_that_stops_before_reporting_releases_its_slot() {
        let mut jobs = Consolidation::default();
        let now = Instant::now();
        assert!(jobs.begin("ene", now));
        assert!(!jobs.begin("ene", now), "one job per companion");
        drop(Report {
            sender: jobs.sender(),
            companion: "ene".into(),
            sent: false,
        });
        let reported = jobs.receiver.try_recv().expect("the guard reports");
        assert!(reported.result.is_err(), "a missing report is a failure");
        assert!(
            reported.batch.is_none(),
            "an early stop has no commit input to report"
        );
        assert_eq!(reported.companion, "ene");
        jobs.fail(&reported.companion, Instant::now());
        assert!(!jobs.running.contains("ene"), "the slot is free again");
        assert!(
            jobs.begin("ene", now + RETRY_DELAY + Duration::from_secs(1)),
            "the next attempt after the retry delay starts"
        );
    }

    // === The job path end to end: start -> extraction -> Report -> drain ===

    fn companion() -> crate::host::mind::Companion {
        use crate::host::json::JsonDoc;
        use crate::host::mind::{Assignment, Assignments};
        let assignment = Assignment {
            connection_id: "c1".into(),
            model_id: "test-model".into(),
        };
        crate::host::mind::Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Default::default(),
            profile_revision: 1,
            state: Default::default(),
            state_revision: 1,
            body: JsonDoc::empty(),
            voice: JsonDoc::empty(),
            assignments: Assignments {
                companion: assignment.clone(),
                task: assignment.clone(),
                consolidation: Some(assignment),
                ..Default::default()
            },
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// A store with one LLM connection and the companion's consolidation
    /// assignment, which is what `start` resolves its model through.
    fn configured(base: &str) -> Store {
        use crate::host::settings::{
            Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        };
        let store = store();
        let revision = store.settings_revision().unwrap();
        store
            .update_settings(
                revision,
                &[(
                    Connection {
                        id: "c1".into(),
                        name: "fixture".into(),
                        enabled: true,
                        kind: ConnectionKind::Llm,
                        config: ConnectionConfig::Llm(LlmConnection {
                            endpoint: Endpoint {
                                adapter: "openrouter".into(),
                                endpoint: base.into(),
                                options_schema: String::new(),
                                options: String::new(),
                            },
                            model_ids: vec!["test-model".into()],
                        }),
                        credential_state: CredentialState::Registered,
                        revision: 1,
                        last_check: None,
                    },
                    0,
                )],
            )
            .unwrap();
        store.insert_companion(&companion()).unwrap();
        store
    }

    struct Setup {
        store: Store,
        credentials: Credentials,
        embeddings: Embeddings,
        models: Models,
        providers: crate::host::providers::Providers,
    }

    fn setup(base: &str) -> Setup {
        let store = configured(base);
        let credentials = Credentials::new(true);
        credentials.remember("c1", "test-key".into());
        Setup {
            store,
            credentials,
            embeddings: Embeddings::default(),
            models: Models::default(),
            providers: crate::host::providers::Providers::none(),
        }
    }

    fn sources<'a>(setup: &'a Setup, runtime: Option<&'a tokio::runtime::Handle>) -> Sources<'a> {
        Sources {
            store: &setup.store,
            runtime,
            embeddings: &setup.embeddings,
            credentials: Some(&setup.credentials),
            models: &setup.models,
            providers: &setup.providers,
        }
    }

    /// A local OpenAI-compatible endpoint that answers one extraction call
    /// with `facts`. `hold` keeps the answer back until the test releases it.
    struct ExtractionServer {
        base: String,
        requested: tokio::sync::oneshot::Receiver<()>,
        release: Arc<tokio::sync::Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn serve_extraction(facts: serde_json::Value, hold: bool) -> ExtractionServer {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (requested, received) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let server_release = Arc::clone(&release);
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
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
                    break;
                }
            }
            eprintln!(
                "SERVER REQUEST: {}",
                String::from_utf8_lossy(&buffer[..buffer.len().min(400)])
            );
            let _ = requested.send(());
            if hold {
                server_release.notified().await;
            }
            let arguments = serde_json::json!({ "facts": facts }).to_string();
            let body = serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": { "name": "submit", "arguments": arguments }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })
            .to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
        });
        ExtractionServer {
            base,
            requested: received,
            release,
            task,
        }
    }

    /// Runs the drain exactly as the host's Update schedule does, until the
    /// companion's slot is released by a real reply.
    async fn drain_until_settled(app: &mut App) {
        let started = Instant::now();
        loop {
            app.update();
            if app.world().resource::<Consolidation>().running.is_empty() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the job did not settle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn drain_app(store: &Store, jobs: Consolidation) -> App {
        let mut app = App::new();
        app.insert_resource(store.clone())
            .insert_resource(jobs)
            .insert_resource(Notifier::new())
            .add_systems(Update, drain_consolidation);
        app
    }

    fn notices(notifier: &mut Notifier) -> Vec<String> {
        notifier.flush();
        let cursor = notifier.cursor();
        let start = ene_rpc::Cursor {
            epoch: cursor.epoch,
            seq: 0,
            ..Default::default()
        };
        notifier
            .replay(&start)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|event| match event.event.as_ref()? {
                ene_rpc::notification::Event::Generic(host) => {
                    host.payload.as_option().map(|payload| payload.json.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn turn() -> Vec<Turn> {
        vec![Turn {
            turn_id: "5".into(),
            text: "ユーザー: 紅茶の店はどこ\n".into(),
        }]
    }

    fn fact() -> serde_json::Value {
        serde_json::json!([{
            "content": "紅茶の店は新宿にある",
            "importance": 7,
            "audience": "",
            "supersedes": "m1",
            "cited_memory_ids": []
        }])
    }

    /// The normal path: the job retrieves a non-empty context, the model
    /// supersedes the existing memory, and the drain commits the batch the
    /// task filled in. An empty context would insert a second row instead.
    #[tokio::test]
    async fn a_job_commits_the_context_it_retrieved_and_updates_the_memory() {
        let server = serve_extraction(fact(), false).await;
        let setup = setup(&server.base);
        let existing = setup
            .store
            .insert_memory(new_memory("m1", "東京都の紅茶の店"))
            .unwrap();
        let mut jobs = Consolidation::default();
        {
            let runtime = tokio::runtime::Handle::current();
            let mut sources = sources(&setup, Some(&runtime));
            start(&mut sources, &mut jobs, "ene", None, turn(), Instant::now());
        }
        let mut app = drain_app(&setup.store, jobs);
        drain_until_settled(&mut app).await;
        let updated = setup.store.memory(&existing.id).unwrap().unwrap();
        assert_eq!(updated.content, "紅茶の店は新宿にある");
        assert_eq!(updated.importance, 7);
        assert_eq!(
            setup.store.consolidation_cursor("ene").unwrap(),
            Some("5".into())
        );
        let count: i64 = setup
            .store
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "the superseded memory is updated, not inserted again"
        );
        assert!(app.world().resource::<Consolidation>().running.is_empty());
        server.task.abort();
    }

    /// The revision the batch captured is checked at commit time: a memory
    /// changed while the model ran turns into a conflict, and neither the
    /// memory nor the cursor moves.
    #[tokio::test]
    async fn a_job_that_loses_the_revision_conflict_rolls_back_memory_and_cursor() {
        let server = serve_extraction(fact(), true).await;
        let setup = setup(&server.base);
        let existing = setup
            .store
            .insert_memory(new_memory("m1", "東京都の紅茶の店"))
            .unwrap();
        let mut jobs = Consolidation::default();
        {
            let runtime = tokio::runtime::Handle::current();
            let mut sources = sources(&setup, Some(&runtime));
            start(&mut sources, &mut jobs, "ene", None, turn(), Instant::now());
        }
        tokio::time::timeout(Duration::from_secs(10), server.requested)
            .await
            .expect("the extraction starts")
            .unwrap();
        // The memory changes while the model is still reading its context.
        setup
            .store
            .control_memory(
                &existing.id,
                crate::host::db::memories::MemoryActionKind::Share,
                Some("other"),
            )
            .unwrap();
        let changed = setup.store.memory(&existing.id).unwrap().unwrap();
        assert_ne!(changed.memory_revision, existing.memory_revision);
        server.release.notify_one();
        let mut app = drain_app(&setup.store, jobs);
        drain_until_settled(&mut app).await;
        let after = setup.store.memory(&existing.id).unwrap().unwrap();
        assert_eq!(after.content, existing.content, "the update rolled back");
        assert_eq!(after.memory_revision, changed.memory_revision);
        assert_eq!(
            setup.store.consolidation_cursor("ene").unwrap(),
            None,
            "the cursor did not advance"
        );
        let jobs = app.world().resource::<Consolidation>();
        assert!(jobs.retry_at.contains_key("ene"), "the failure is retried");
        assert!(jobs.running.is_empty());
        let mut app = app;
        let mut notifier = app.world_mut().resource_mut::<Notifier>();
        let notices = notices(&mut notifier);
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("memory changed while consolidation was running")),
            "{notices:?}"
        );
        server.task.abort();
    }

    /// A task dropped before it reports (the owner went away) still frees the
    /// slot through the drop guard and reports a failure the drain records.
    #[tokio::test]
    async fn a_job_dropped_before_it_reports_releases_the_slot_and_retries() {
        let server = serve_extraction(serde_json::json!([]), true).await;
        let setup = setup(&server.base);
        let mut jobs = Consolidation::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let handle = runtime.handle().clone();
            let mut sources = sources(&setup, Some(&handle));
            start(&mut sources, &mut jobs, "ene", None, turn(), Instant::now());
        }
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), server.requested)
                    .await
                    .expect("the extraction starts")
                    .unwrap();
            });
            // The owner goes away while the task is waiting for the provider.
            runtime.shutdown_timeout(Duration::from_millis(100));
        })
        .await
        .unwrap();
        let mut app = drain_app(&setup.store, jobs);
        drain_until_settled(&mut app).await;
        {
            let jobs = app.world().resource::<Consolidation>();
            assert!(!jobs.running.contains("ene"), "the slot is released");
            assert!(jobs.retry_at.contains_key("ene"));
        }
        {
            let mut notifier = app.world_mut().resource_mut::<Notifier>();
            let notices = notices(&mut notifier);
            assert!(
                notices
                    .iter()
                    .any(|notice| notice
                        .contains("the consolidation task stopped before it reported")),
                "{notices:?}"
            );
        }
        assert!(
            app.world_mut()
                .resource_mut::<Consolidation>()
                .begin("ene", Instant::now() + RETRY_DELAY + Duration::from_secs(1)),
            "a retry can start after the delay"
        );
        server.task.abort();
    }

    fn new_memory(id: &str, content: &str) -> MemoryRecord {
        MemoryRecord {
            id: id.into(),
            owner: "ene".into(),
            audiences: Vec::new(),
            content: content.into(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    fn context(memory: &crate::host::mind::memory::Memory) -> MemoryContext {
        MemoryContext {
            id: memory.id.clone(),
            revision: memory.memory_revision,
            importance: memory.importance,
            content: memory.content.clone(),
        }
    }

    #[tokio::test]
    async fn direct_extractor_preserves_the_entire_request_and_cache_defaults() {
        use rig_core::test_utils::{MockCompletionModel, MockTurn};
        use rig_core::{catalog::Catalog, completion::CacheRetention};

        let mut spec = Catalog::builtin()
            .resolve("openai/gpt-4o-mini")
            .unwrap()
            .shared();
        std::sync::Arc::make_mut(&mut spec).caching.retention = vec![CacheRetention::Short];
        let mock = MockCompletionModel::from_turns(
            (0..2)
                .map(|_| MockTurn::tool_call("facts", "submit", serde_json::json!({"facts": []}))),
        )
        .priced_as(spec);
        let model: DynModel<Completion> = mock.clone().into();
        let options = model.spec().unwrap().default_options(None);
        assert_eq!(options.cache, Some(CacheRetention::Short));
        ExtractorBuilder::<Extraction>::from_agent_builder(
            rig_agent::agent::AgentBuilder::new(model.clone()).options(options),
        )
        .append_preamble(SYSTEM_PROMPT)
        .retries(0)
        .build()
        .extract("The user prefers tea.")
        .await
        .unwrap();
        assert!(
            extract(&model, "The user prefers tea.".into())
                .await
                .unwrap()
                .is_empty()
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert!(
            requests[1]
                .system_instructions()
                .unwrap()
                .contains(SYSTEM_PROMPT)
        );
        assert_eq!(requests[1].options.cache, Some(CacheRetention::Short));
    }

    #[tokio::test]
    async fn extraction_submits_bounded_facts_and_rejects_free_text() {
        use rig_core::test_utils::{MockCompletionModel, MockTurn};
        let model: DynModel<Completion> = MockCompletionModel::from_turns([
            MockTurn::tool_call("facts", "submit", serde_json::json!({
                "facts": [{"content": "  prefers tea  ", "importance": 7,
                    "audience": "", "supersedes": null, "cited_memory_ids": ["injected", "ghost"]}]
            })),
            MockTurn::text("```json\n{\"facts\": []}\n```"),
        ]).into();
        let facts = extract(&model, "The user prefers tea.".into())
            .await
            .unwrap();
        assert_eq!(facts[0].content, "prefers tea");
        assert_eq!(facts[0].importance, Some(7));
        assert_eq!(
            validate_citations(&facts[0].cited_memory_ids, &["injected".into()]),
            ["injected"]
        );
        assert!(extract(&model, "No facts.".into()).await.is_err());
    }

    #[test]
    fn failed_jobs_keep_the_durable_range_pending_and_retry_only_their_companion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        let mut conversation = CoreConversation::new();
        let first = answer(&mut conversation, "first", "answered");
        let next = answer(&mut conversation, "next", "answered");
        store
            .consolidate(&batch(None, &first.get().to_string()), &[])
            .unwrap();
        let now = Instant::now();
        let mut jobs = Consolidation::default();
        assert!(jobs.begin("ene", now));
        assert!(!jobs.begin("ene", now));
        assert!(jobs.begin("other", now));
        jobs.fail("ene", now);
        assert!(!jobs.begin("ene", now + RETRY_DELAY - Duration::from_nanos(1)));
        assert!(jobs.running.contains("other"));
        assert!(jobs.begin("ene", now + RETRY_DELAY));
        drop(jobs);
        drop(store);

        let store = Store::open(&path).unwrap();
        let mut restarted = Consolidation::default();
        assert!(restarted.begin("ene", now));
        let cursor = store.consolidation_cursor("ene").unwrap();
        let pending = pending_turns(&conversation, cursor.as_deref(), MAX_TURNS);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].turn_id, next.get().to_string());
    }

    #[test]
    fn pending_turns_reads_only_answered_turns_after_the_cursor() {
        let mut conversation = CoreConversation::new();
        let first = answer(&mut conversation, "hello", "hi");
        let second = answer(&mut conversation, "my name is A", "noted");
        // A turn that never ended is not consolidated.
        conversation.push(Message::user("unfinished"));
        conversation.open_turn();
        conversation.push(Message::assistant("cut off"));

        let turns = pending_turns(&conversation, None, 8);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_id, first.get().to_string());
        assert_eq!(turns[1].turn_id, second.get().to_string());
        assert!(turns[1].text.contains("my name is A"));
        let after = pending_turns(&conversation, Some(&first.get().to_string()), 8);
        assert_eq!(after.len(), 1);
        assert!(pending_turns(&conversation, Some("not-a-turn"), 8).is_empty());
    }

    #[test]
    fn a_stale_cursor_or_memory_revision_aborts_the_whole_commit() {
        let store = store();
        store.consolidate(&batch(None, "5"), &[]).unwrap();
        assert!(
            matches!(
                store.consolidate(&batch(None, "5"), &[]),
                Err(HostError::Conflict(_))
            ),
            "a second first commit must not duplicate the range"
        );
        store.consolidate(&batch(Some("5"), "7"), &[]).unwrap();
        assert!(matches!(
            store.consolidate(&batch(Some("5"), "9"), &[]),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(
            store.consolidation_cursor("ene").unwrap(),
            Some("7".to_owned())
        );

        let memory = store.insert_memory(new_memory("m1", "tea")).unwrap();
        let mut batch = batch(Some("7"), "9");
        batch.context = vec![context(&memory)];
        // A concurrent correction moves the revision the job captured.
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE memories SET memory_revision = memory_revision + 1 WHERE id = 'm1'",
                [],
            )
            .unwrap();
        let facts = vec![Fact {
            content: "green tea".into(),
            importance: Some(7),
            supersedes: Some("m1".into()),
            ..Fact::default()
        }];
        assert!(matches!(
            store.consolidate(&batch, &facts),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(
            store.consolidation_cursor("ene").unwrap(),
            Some("7".to_owned())
        );
        assert_eq!(store.memory("m1").unwrap().unwrap().content, "tea");
    }

    #[test]
    fn a_valid_job_merges_cites_and_advances_the_cursor() {
        let store = store();
        let memory = store.insert_memory(new_memory("m2", "coffee")).unwrap();
        let mut batch = batch(None, "8");
        batch.context = vec![context(&memory)];
        batch.injected = vec!["m2".into()];
        let facts = vec![Fact {
            content: "A prefers coffee".into(),
            importance: Some(7),
            supersedes: Some("m2".into()),
            cited_memory_ids: vec!["m2".into(), "ghost".into()],
            ..Fact::default()
        }];
        let commit = store.consolidate(&batch, &facts).unwrap();
        assert_eq!(commit.updated, 1);
        let updated = store.memory("m2").unwrap().unwrap();
        assert_eq!(updated.content, "A prefers coffee");
        assert_eq!(updated.importance, 7);
        assert_eq!(store.consolidation_cursor("ene").unwrap(), Some("8".into()));
        assert!(
            store
                .memory_edges("m2")
                .unwrap()
                .iter()
                .any(|(relation, to)| relation == "estimated_cited_in" && to == "8")
        );
        // The ghost citation had no injected row to update.
        let citations: i64 = store
            .conn()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE relation = 'estimated_cited_in'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(citations, 1);
    }
}
