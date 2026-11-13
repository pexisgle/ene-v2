//! Incremental consolidation: answered turns are read after the stored
//! cursor, a background model extracts structured facts, the host validates
//! citations and commits memories, edges and the new cursor in one SQLite
//! transaction (docs/design/mind.md §5.2).
//!
//! Only the model call runs on the background task owned by `mind::embedding`;
//! its result comes back to the ECS thread and is committed there.
use super::{
    Companion, Role,
    embedding::{self, Embedder},
};
use crate::host::{
    agent::CompanionAgent,
    db::{Result, Store},
    error::HostError,
    model::Credentials,
    notify::Notifier,
};
use bevy_ecs::prelude::*;
use rig_core::completion::Message;
use rig_core::conversation::{Conversation as CoreConversation, TurnEnd};
use rig_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
const LLM_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Fact {
    #[serde(default)]
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) importance: Option<u8>,
    #[serde(default)]
    pub(crate) audience: Option<String>,
    #[serde(default)]
    pub(crate) supersedes: Option<String>,
    #[serde(default)]
    pub(crate) cited_memory_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Extraction {
    #[serde(default)]
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

#[derive(Clone, Debug)]
struct LlmTarget {
    endpoint: String,
    model_id: String,
    secret: String,
}

/// Resolves the companion's `Consolidation` assignment (or its documented
/// companion fallback) to a chat-completions target.
fn llm_target(
    store: &Store,
    credentials: Option<&Credentials>,
    companion: &Companion,
) -> Result<Option<LlmTarget>> {
    let Some(assignment) = companion.model(Role::Consolidation) else {
        return Ok(None);
    };
    let Some(connection) = store.connection(&assignment.connection_id)? else {
        return Ok(None);
    };
    if !connection.enabled || connection.kind != crate::host::settings::ConnectionKind::Llm {
        return Ok(None);
    }
    let (Some(endpoint), Some(secret)) = (
        embedding::connection_endpoint(&connection),
        embedding::connection_secret(&connection, credentials),
    ) else {
        return Ok(None);
    };
    Ok(Some(LlmTarget {
        endpoint,
        model_id: assignment.model_id.clone(),
        secret,
    }))
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

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object", "additionalProperties": false, "required": ["facts"],
        "properties": {"facts": {"type": "array", "items": {
            "type": "object", "additionalProperties": false,
            "required": ["content", "importance", "audience", "supersedes", "cited_memory_ids"],
            "properties": {
                "content": {"type": "string", "maxLength": 4000},
                "importance": {"type": "integer", "minimum": 0, "maximum": 10},
                "audience": {"type": ["string", "null"], "maxLength": 128},
                "supersedes": {"type": ["string", "null"], "maxLength": 128},
                "cited_memory_ids": {
                    "type": "array", "items": {"type": "string", "maxLength": 128}
                }
            }
        }}}
    })
}

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

/// Parses the model's last message into facts. Fences and surrounding prose
/// are tolerated, but the JSON itself must validate.
pub(crate) fn parse_extraction(text: &str) -> Result<Vec<Fact>> {
    let body = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let start = body
        .find('{')
        .ok_or_else(|| HostError::unavailable("consolidation output has no JSON object"))?;
    let end = body
        .rfind('}')
        .ok_or_else(|| HostError::unavailable("consolidation output has no JSON object"))?;
    let body = body
        .get(start..=end)
        .ok_or_else(|| HostError::unavailable("consolidation output is not valid JSON"))?;
    let extraction: Extraction = serde_json::from_str(body)
        .map_err(|error| HostError::unavailable(format!("consolidation output: {error}")))?;
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
    Ok(facts)
}

async fn extract(target: LlmTarget, prompt: String) -> Result<Vec<Fact>> {
    let body = serde_json::json!({
        "model": target.model_id,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ],
        "temperature": 0.0,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "ene.consolidation.v1",
                "strict": true,
                "schema": schema(),
            }
        }
    });
    let value = embedding::post_json(
        &format!("{}/chat/completions", target.endpoint.trim_end_matches('/')),
        &body,
        &target.secret,
        LLM_TIMEOUT,
        MAX_RESPONSE_BYTES,
        "consolidation",
    )
    .await?;
    let content = value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| HostError::unavailable("consolidation body has no message content"))?;
    parse_extraction(content)
}

/// Memory ids the host injected into recent turns, recorded by recall. The
/// citation guard can only accept ids found here.
#[derive(Resource, Default)]
pub(crate) struct InjectedMemories {
    by_companion: HashMap<String, VecDeque<(String, Vec<String>)>>,
}

impl InjectedMemories {
    const KEEP: usize = 64;

    /// Records one turn's injected ids. Recording a turn twice replaces it.
    pub(crate) fn record(&mut self, companion: &str, turn_id: &str, ids: Vec<String>) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| !id.is_empty())
            .take(MAX_CITATIONS)
            .collect();
        let turns = self.by_companion.entry(companion.to_owned()).or_default();
        if let Some(existing) = turns.iter_mut().find(|(turn, _)| turn == turn_id) {
            existing.1 = ids;
            return;
        }
        turns.push_back((turn_id.to_owned(), ids));
        while turns.len() > Self::KEEP {
            turns.pop_front();
        }
    }

    /// The ids injected into turns after `after` (exclusive) through
    /// `through` (inclusive). Ids of unreadable turn numbers are kept, since
    /// dropping them would weaken the guard.
    pub(crate) fn ids_between(
        &self,
        companion: &str,
        after: Option<&str>,
        through: &str,
    ) -> Vec<String> {
        let lower = after.and_then(|after| after.parse::<u64>().ok());
        let upper = through.parse::<u64>().ok();
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        let Some(turns) = self.by_companion.get(companion) else {
            return ids;
        };
        for (turn, injected) in turns {
            let Ok(turn) = turn.parse::<u64>() else {
                continue;
            };
            if upper.is_some_and(|upper| turn > upper) {
                continue;
            }
            if lower.is_some_and(|lower| turn <= lower) {
                continue;
            }
            for id in injected {
                if seen.insert(id.as_str()) {
                    ids.push(id.clone());
                }
            }
        }
        ids
    }
}

/// What the background extraction task posts back to the ECS thread. The
/// batch travels with the result because it holds the commit input.
type Reply = (Batch, std::result::Result<Vec<Fact>, String>);

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
        let (sender, receiver) = crossbeam_channel::bounded(16);
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

/// Registers the injection ledger, the guard and the two background systems.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<Consolidation>()
        .init_resource::<InjectedMemories>()
        .add_systems(
            Update,
            (schedule_consolidation, drain_consolidation)
                .chain()
                .after(AgentLoop),
        );
}

/// The sources one job reads; bundled so `start` stays a normal function.
struct Sources<'a> {
    store: &'a Store,
    embedder: &'a Embedder,
    credentials: Option<&'a Credentials>,
    injected: &'a InjectedMemories,
}

fn schedule_consolidation(
    settled: Query<(&TurnOutcome, &ForAgent), Added<Settled>>,
    agents: Query<(&CompanionAgent, &Conversation)>,
    store: Res<Store>,
    embedder: Res<Embedder>,
    credentials: Option<Res<Credentials>>,
    injected: Res<InjectedMemories>,
    mut jobs: ResMut<Consolidation>,
) {
    let now = Instant::now();
    let sources = Sources {
        store: &store,
        embedder: &embedder,
        credentials: credentials.as_deref(),
        injected: &injected,
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
            start(&sources, &mut jobs, &companion, cursor, pending, now);
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
        start(&sources, &mut jobs, companion, cursor, pending, now);
    }
}

fn start(
    sources: &Sources<'_>,
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
    let target = match llm_target(sources.store, sources.credentials, &companion) {
        Ok(Some(target)) => target,
        Ok(None) | Err(_) => {
            jobs.fail(companion_id, now);
            return;
        }
    };
    let context = match sources
        .store
        .consolidation_context(companion_id, MAX_CONTEXT)
    {
        Ok(context) => context,
        Err(_) => {
            jobs.fail(companion_id, now);
            return;
        }
    };
    let injected = sources
        .injected
        .ids_between(companion_id, job_start.as_deref(), &new_cursor);
    let prompt = prompt(&pending, &context, &injected);
    let batch = Batch {
        companion_id: companion_id.to_owned(),
        job_start,
        new_cursor,
        context,
        injected,
    };
    let sender = jobs.sender();
    let queued = sources.embedder.spawn(async move {
        let result = extract(target, prompt)
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send((batch, result));
    });
    if !queued {
        jobs.fail(companion_id, now);
    }
}

fn drain_consolidation(
    store: Res<Store>,
    mut jobs: ResMut<Consolidation>,
    mut notifier: Option<ResMut<Notifier>>,
) {
    while let Ok((batch, result)) = jobs.receiver.try_recv() {
        let message = match result {
            Ok(facts) => match store.consolidate(&batch, &facts) {
                Ok(commit) => {
                    jobs.succeed(&batch.companion_id);
                    tracing::debug!(
                        companion = %batch.companion_id,
                        inserted = commit.inserted,
                        updated = commit.updated,
                        cursor = %commit.cursor,
                        "consolidation committed"
                    );
                    continue;
                }
                Err(error) => error.to_string(),
            },
            Err(error) => error,
        };
        jobs.fail(&batch.companion_id, Instant::now());
        if let Some(notifier) = notifier.as_mut() {
            notifier.notice(
                Some(&batch.companion_id),
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
