//! Incremental consolidation: answered turns are read after the stored
//! cursor, a background model extracts structured facts, the host validates
//! citations and commits memories, edges and the new cursor in one SQLite
//! transaction (docs/design/mind.md §5.2).
//!
//! The network call runs on a Tokio task owned by `mind::embedding`; only the
//! result comes back to the ECS thread through `ConsolidationJobs`, so a slow
//! model never blocks a frame.
use super::{
    Companion, Role,
    embedding::{self, Embeddings},
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
pub(crate) const MAX_TURNS: usize = 20;
/// Unprocessed turns that force a job without waiting for idle.
pub(crate) const TURN_THRESHOLD: usize = 5;
/// How long the conversation must be quiet before an idle job runs.
pub(crate) const IDLE_WINDOW: Duration = Duration::from_secs(60);
/// Failed jobs wait this long before the next settle or idle retries them.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(30);
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_FACTS: usize = 32;
const MAX_CONTEXT: usize = 20;
const MAX_CITATIONS: usize = 64;
const LLM_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// One answered turn as data: the id the host uses in notifications and the
/// text the extraction model reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConsolidationTurn {
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

/// Everything one commit needs, captured when the job started.
#[derive(Clone, Debug)]
pub(crate) struct ConsolidationBatch {
    pub(crate) companion_id: String,
    /// The cursor read at job start; `None` when no cursor row exists yet.
    pub(crate) job_start: Option<String>,
    /// The cursor this commit advances to.
    pub(crate) new_cursor: String,
    pub(crate) turns: Vec<ConsolidationTurn>,
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConsolidationCommit {
    pub(crate) inserted: Vec<String>,
    pub(crate) updated: Vec<String>,
    pub(crate) forgotten: Vec<String>,
    pub(crate) citations: Vec<String>,
    pub(crate) rejected_citations: usize,
    pub(crate) cursor: String,
}

/// The answered turns after `cursor`, in conversation order.
pub(crate) fn pending_turns(
    conversation: &CoreConversation,
    cursor: Option<&str>,
    limit: usize,
) -> Vec<ConsolidationTurn> {
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
            .map(|(_, next)| conversation.start_of(next.start))
            .unwrap_or_else(|| conversation.entries().len());
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
        turns.push(ConsolidationTurn {
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
    secret: Option<String>,
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
    let Some(endpoint) = embedding::connection_endpoint(&connection) else {
        return Ok(None);
    };
    let Some(secret) = embedding::connection_secret(&connection, credentials) else {
        return Ok(None);
    };
    Ok(Some(LlmTarget {
        endpoint,
        model_id: assignment.model_id.clone(),
        secret: Some(secret),
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
        "type": "object",
        "additionalProperties": false,
        "required": ["facts"],
        "properties": {
            "facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "content",
                        "importance",
                        "audience",
                        "supersedes",
                        "cited_memory_ids"
                    ],
                    "properties": {
                        "content": {"type": "string", "maxLength": 4000},
                        "importance": {"type": "integer", "minimum": 0, "maximum": 10},
                        "audience": {"type": ["string", "null"], "maxLength": 128},
                        "supersedes": {"type": ["string", "null"], "maxLength": 128},
                        "cited_memory_ids": {
                            "type": "array",
                            "items": {"type": "string", "maxLength": 128}
                        }
                    }
                }
            }
        }
    })
}

/// Renders the extraction prompt. It carries the transcript, the candidate
/// memories with their revisions, and the ids the host injected.
pub(crate) fn prompt(batch: &ConsolidationBatch) -> String {
    let mut prompt = String::from("以下から長期記憶にする事実を抽出してください。\n\n");
    prompt.push_str("既存記憶（id / revision / importance / 内容）:\n");
    if batch.context.is_empty() {
        prompt.push_str("- なし\n");
    }
    for memory in &batch.context {
        prompt.push_str(&format!(
            "- id={} revision={} importance={} content={}\n",
            memory.id, memory.revision, memory.importance, memory.content
        ));
    }
    prompt.push_str("\n注入済み記憶ID（cited_memory_ids に書けるのはこれだけ）:\n");
    if batch.injected.is_empty() {
        prompt.push_str("- なし\n");
    }
    for id in &batch.injected {
        prompt.push_str(&format!("- {id}\n"));
    }
    prompt.push_str("\n会話（確定済みターン）:\n");
    for turn in &batch.turns {
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
    let mut body = text.trim();
    if let Some(rest) = body.strip_prefix("```") {
        body = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest);
        body = body.trim_start();
        if let Some(rest) = body.strip_suffix("```") {
            body = rest.trim_end();
        }
    }
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
    let url = format!("{}/chat/completions", target.endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(LLM_TIMEOUT)
        .build()
        .map_err(|error| HostError::unavailable(format!("consolidation client: {error}")))?;
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
    let mut request = client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body);
    if let Some(secret) = &target.secret {
        request = request.bearer_auth(secret);
    }
    let response = request
        .send()
        .await
        .map_err(|error| HostError::unavailable(format!("consolidation endpoint: {error}")))?;
    if !response.status().is_success() {
        return Err(HostError::unavailable(format!(
            "consolidation endpoint answered {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| HostError::unavailable(format!("consolidation body: {error}")))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(HostError::unavailable(
            "consolidation endpoint answered more data than the host reads",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| HostError::unavailable(format!("consolidation body: {error}")))?;
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

/// What a background extraction task posts back to the ECS thread.
#[derive(Clone, Debug)]
pub(crate) enum ConsolidationReply {
    Done {
        batch: Box<ConsolidationBatch>,
        facts: Vec<Fact>,
    },
    Failed {
        companion_id: String,
        error: String,
    },
}

/// The single-flight guard, retry schedule and result channel per companion.
#[derive(Resource)]
pub(crate) struct ConsolidationJobs {
    running: HashSet<String>,
    retry_at: HashMap<String, Instant>,
    errors: HashMap<String, String>,
    sender: crossbeam_channel::Sender<ConsolidationReply>,
    receiver: crossbeam_channel::Receiver<ConsolidationReply>,
}

impl Default for ConsolidationJobs {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(16);
        Self {
            running: HashSet::new(),
            retry_at: HashMap::new(),
            errors: HashMap::new(),
            sender,
            receiver,
        }
    }
}

impl ConsolidationJobs {
    /// Claims the companion's only slot. `false` means a job is still running
    /// or a failure is in its retry delay.
    pub(crate) fn begin(&mut self, companion: &str, now: Instant) -> bool {
        if self.running.contains(companion) {
            return false;
        }
        if self
            .retry_at
            .get(companion)
            .is_some_and(|retry| *retry > now)
        {
            return false;
        }
        self.running.insert(companion.to_owned());
        true
    }

    pub(crate) fn finish(&mut self, companion: &str) {
        self.running.remove(companion);
    }

    /// Whether this companion's one slot is busy.
    pub(crate) fn is_running(&self, companion: &str) -> bool {
        self.running.contains(companion)
    }

    pub(crate) fn succeed(&mut self, companion: &str) {
        self.running.remove(companion);
        self.retry_at.remove(companion);
        self.errors.remove(companion);
    }

    pub(crate) fn fail(&mut self, companion: &str, error: impl Into<String>, now: Instant) {
        self.running.remove(companion);
        self.retry_at
            .insert(companion.to_owned(), now + RETRY_DELAY);
        self.errors.insert(companion.to_owned(), error.into());
    }

    #[allow(dead_code)]
    pub(crate) fn last_error(&self, companion: &str) -> Option<&str> {
        self.errors.get(companion).map(String::as_str)
    }

    fn sender(&self) -> crossbeam_channel::Sender<ConsolidationReply> {
        self.sender.clone()
    }

    fn try_recv(&self) -> Option<ConsolidationReply> {
        self.receiver.try_recv().ok()
    }
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
    #[allow(dead_code)]
    pub(crate) fn record(&mut self, companion: &str, turn_id: &str, ids: Vec<String>) {
        let mut seen = HashSet::new();
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| !id.is_empty() && seen.insert(id.clone()))
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

/// When each companion last settled a turn, and when the idle scan last ran.
#[derive(Resource, Default)]
pub(crate) struct ConsolidationActivity {
    settled_at: HashMap<String, Instant>,
    scanned_at: Option<Instant>,
}

/// Registers the guard, the injection ledger and the two background systems.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<ConsolidationJobs>()
        .init_resource::<InjectedMemories>()
        .init_resource::<ConsolidationActivity>()
        .add_systems(
            Update,
            (schedule_consolidation, drain_consolidation)
                .chain()
                .after(AgentLoop),
        );
}

#[allow(clippy::too_many_arguments)]
fn schedule_consolidation(
    settled: Query<(&TurnOutcome, &ForAgent), Added<Settled>>,
    agents: Query<(&CompanionAgent, &Conversation)>,
    store: Res<Store>,
    embeddings: Res<Embeddings>,
    credentials: Option<Res<Credentials>>,
    injected: Res<InjectedMemories>,
    mut jobs: ResMut<ConsolidationJobs>,
    mut activity: ResMut<ConsolidationActivity>,
) {
    let now = Instant::now();
    let mut answered: HashSet<String> = HashSet::new();
    for (outcome, of) in &settled {
        let TurnOutcome::Answered(_) = outcome else {
            continue;
        };
        let Ok((agent, conversation)) = agents.get(of.0) else {
            continue;
        };
        let companion = agent.companion_id.clone();
        activity.settled_at.insert(companion.clone(), now);
        answered.insert(companion.clone());
        let Ok(cursor) = store.consolidation_cursor(&companion) else {
            continue;
        };
        let pending = pending_turns(conversation, cursor.as_deref(), TURN_THRESHOLD);
        if pending.len() >= TURN_THRESHOLD {
            start(
                &store,
                &embeddings,
                credentials.as_deref(),
                &injected,
                &mut jobs,
                &companion,
                conversation,
                cursor,
                now,
            );
        }
    }
    let scan = activity
        .scanned_at
        .is_none_or(|last| now.duration_since(last) >= SCAN_INTERVAL);
    if !scan {
        return;
    }
    activity.scanned_at = Some(now);
    for (agent, conversation) in &agents {
        let companion = &agent.companion_id;
        if answered.contains(companion) || jobs.is_running(companion) {
            continue;
        }
        let idle = activity
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
        start(
            &store,
            &embeddings,
            credentials.as_deref(),
            &injected,
            &mut jobs,
            companion,
            conversation,
            cursor,
            now,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn start(
    store: &Store,
    embeddings: &Embeddings,
    credentials: Option<&Credentials>,
    injected: &InjectedMemories,
    jobs: &mut ConsolidationJobs,
    companion_id: &str,
    conversation: &Conversation,
    job_start: Option<String>,
    now: Instant,
) {
    if !jobs.begin(companion_id, now) {
        return;
    }
    let pending = pending_turns(conversation, job_start.as_deref(), MAX_TURNS);
    let Some(new_cursor) = pending.last().map(|turn| turn.turn_id.clone()) else {
        jobs.finish(companion_id);
        return;
    };
    let companion = match store.companion(companion_id) {
        Ok(Some(companion)) => companion,
        Ok(None) => {
            jobs.finish(companion_id);
            return;
        }
        Err(error) => {
            jobs.fail(companion_id, error.to_string(), now);
            return;
        }
    };
    let target = match llm_target(store, credentials, &companion) {
        Ok(Some(target)) => target,
        Ok(None) => {
            jobs.fail(companion_id, "no consolidation model is assigned", now);
            return;
        }
        Err(error) => {
            jobs.fail(companion_id, error.to_string(), now);
            return;
        }
    };
    let context = match store.consolidation_context(companion_id, MAX_CONTEXT) {
        Ok(context) => context,
        Err(error) => {
            jobs.fail(companion_id, error.to_string(), now);
            return;
        }
    };
    let injected = injected.ids_between(companion_id, job_start.as_deref(), &new_cursor);
    let batch = ConsolidationBatch {
        companion_id: companion_id.to_owned(),
        job_start,
        new_cursor,
        turns: pending,
        context,
        injected,
    };
    let prompt = prompt(&batch);
    let sender = jobs.sender();
    let queued = embeddings.spawn(async move {
        let reply = match extract(target, prompt).await {
            Ok(facts) => ConsolidationReply::Done {
                batch: Box::new(batch),
                facts,
            },
            Err(error) => ConsolidationReply::Failed {
                companion_id: batch.companion_id.clone(),
                error: error.to_string(),
            },
        };
        let _ = sender.send(reply);
    });
    if !queued {
        jobs.fail(companion_id, "the background runtime is unavailable", now);
    }
}

fn drain_consolidation(
    store: Res<Store>,
    mut jobs: ResMut<ConsolidationJobs>,
    notifier: Option<ResMut<Notifier>>,
) {
    let mut notifier = notifier;
    let mut notices: Vec<(String, String)> = Vec::new();
    while let Some(reply) = jobs.try_recv() {
        match reply {
            ConsolidationReply::Done { batch, facts } => {
                match store.commit_consolidation(&batch, &facts) {
                    Ok(outcome) => {
                        jobs.succeed(&batch.companion_id);
                        tracing::debug!(
                            companion = %batch.companion_id,
                            inserted = outcome.inserted.len(),
                            updated = outcome.updated.len(),
                            cursor = %outcome.cursor,
                            "consolidation committed"
                        );
                    }
                    Err(error) => {
                        jobs.fail(&batch.companion_id, error.to_string(), Instant::now());
                        notices.push((batch.companion_id, error.to_string()));
                    }
                }
            }
            ConsolidationReply::Failed {
                companion_id,
                error,
            } => {
                jobs.fail(&companion_id, error.clone(), Instant::now());
                notices.push((companion_id, error));
            }
        }
    }
    if let Some(notifier) = notifier.as_mut() {
        for (companion, message) in notices {
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
    use crate::host::db::memories::NewMemory;
    use rig_core::conversation::MessageId;

    fn answer(conversation: &mut CoreConversation, user: &str, reply: &str) -> MessageId {
        let start = conversation.push(Message::user(user));
        conversation.open_turn();
        conversation.push(Message::assistant(reply));
        conversation.end_turn(TurnEnd::Answered { answer: None });
        start
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
        assert_eq!(after[0].turn_id, second.get().to_string());
        assert!(pending_turns(&conversation, Some("not-a-turn"), 8).is_empty());
        assert!(pending_turns(&conversation, Some("999"), 8).is_empty());
    }

    #[test]
    fn the_injection_ledger_only_returns_the_turn_range() {
        let mut ledger = InjectedMemories::default();
        ledger.record("ene", "2", vec!["a".into()]);
        ledger.record("ene", "4", vec!["b".into(), "a".into()]);
        ledger.record("ene", "6", vec!["c".into()]);
        assert_eq!(ledger.ids_between("ene", None, "4"), vec!["a", "b"]);
        assert_eq!(ledger.ids_between("ene", Some("2"), "5"), vec!["b", "a"]);
        assert_eq!(ledger.ids_between("ene", Some("5"), "6"), vec!["c"]);
        // Recording the same turn replaces its ids.
        ledger.record("ene", "2", vec!["d".into()]);
        assert_eq!(ledger.ids_between("ene", None, "2"), vec!["d"]);
        assert!(ledger.ids_between("other", None, "9").is_empty());
    }

    #[test]
    fn citations_are_limited_to_injected_ids() {
        let injected = vec!["a".to_owned(), "b".to_owned()];
        let cited = vec![
            "a".to_owned(),
            "ghost".to_owned(),
            "b".to_owned(),
            "a".to_owned(),
        ];
        assert_eq!(validate_citations(&cited, &injected), vec!["a", "b"]);
        assert!(validate_citations(&["ghost".to_owned()], &injected).is_empty());
    }

    #[test]
    fn extraction_parses_facts_and_tolerates_fences() {
        let text = r#"```json
        {"facts":[
          {"content":"A prefers tea","importance":6,"audience":"*",
           "supersedes":null,"cited_memory_ids":["m1"]},
          {"content":"","importance":3,"audience":null,"supersedes":null,"cited_memory_ids":[]}
        ]}
        ```"#;
        let facts = parse_extraction(text).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "A prefers tea");
        assert_eq!(facts[0].importance, Some(6));
        assert!(parse_extraction("no json here").is_err());
    }

    #[test]
    fn the_single_flight_guard_skips_and_retries() {
        let mut jobs = ConsolidationJobs::default();
        let now = Instant::now();
        assert!(jobs.begin("ene", now));
        assert!(!jobs.begin("ene", now));
        jobs.fail("ene", "boom", now);
        assert!(!jobs.begin("ene", now));
        assert!(jobs.last_error("ene").is_some());
        assert!(jobs.begin("ene", now + RETRY_DELAY + Duration::from_secs(1)));
        jobs.succeed("ene");
        // succeed clears the retry slot and the error.
        assert!(jobs.last_error("ene").is_none());
        assert!(jobs.begin("ene", now));
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn batch(cursor: Option<&str>, next: &str) -> ConsolidationBatch {
        ConsolidationBatch {
            companion_id: "ene".into(),
            job_start: cursor.map(ToOwned::to_owned),
            new_cursor: next.into(),
            turns: Vec::new(),
            context: Vec::new(),
            injected: Vec::new(),
        }
    }

    #[test]
    fn a_stale_cursor_commit_is_rejected() {
        let store = store();
        store.commit_consolidation(&batch(None, "5"), &[]).unwrap();
        assert_eq!(
            store.consolidation_cursor("ene").unwrap(),
            Some("5".to_owned())
        );
        assert!(
            matches!(
                store.commit_consolidation(&batch(None, "5"), &[]),
                Err(HostError::Conflict(_))
            ),
            "a second first commit must not duplicate the range"
        );
        store
            .commit_consolidation(&batch(Some("5"), "7"), &[])
            .unwrap();
        assert!(
            matches!(
                store.commit_consolidation(&batch(Some("5"), "9"), &[]),
                Err(HostError::Conflict(_))
            ),
            "a commit from the old cursor must be rejected"
        );
        assert_eq!(
            store.consolidation_cursor("ene").unwrap(),
            Some("7".to_owned())
        );
    }

    #[test]
    fn a_stale_memory_revision_aborts_the_whole_commit() {
        let store = store();
        let memory = store
            .insert_memory(NewMemory {
                id: "m1".into(),
                owner: "ene".into(),
                audiences: Vec::new(),
                content: "tea".into(),
                importance: 5,
                occurred_at: None,
                source_context: serde_json::json!({}),
            })
            .unwrap();
        let mut batch = batch(None, "5");
        batch.context = vec![MemoryContext {
            id: memory.id.clone(),
            revision: memory.memory_revision,
            importance: memory.importance,
            content: memory.content.clone(),
        }];
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
            store.commit_consolidation(&batch, &facts),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(store.consolidation_cursor("ene").unwrap(), None);
        assert_eq!(store.memory("m1").unwrap().unwrap().content, "tea");
    }

    #[test]
    fn a_valid_job_merges_and_advances_the_cursor() {
        let store = store();
        let memory = store
            .insert_memory(NewMemory {
                id: "m1".into(),
                owner: "ene".into(),
                audiences: Vec::new(),
                content: "tea".into(),
                importance: 5,
                occurred_at: None,
                source_context: serde_json::json!({}),
            })
            .unwrap();
        let mut batch = batch(None, "5");
        batch.context = vec![MemoryContext {
            id: memory.id.clone(),
            revision: memory.memory_revision,
            importance: memory.importance,
            content: memory.content.clone(),
        }];
        batch.injected = vec!["m1".into()];
        let facts = vec![Fact {
            content: "A prefers tea".into(),
            importance: Some(7),
            supersedes: Some("m1".into()),
            cited_memory_ids: vec!["m1".into(), "ghost".into()],
            ..Fact::default()
        }];
        let outcome = store.commit_consolidation(&batch, &facts).unwrap();
        assert_eq!(outcome.updated, vec!["m1"]);
        assert_eq!(outcome.citations, vec!["m1"]);
        assert_eq!(outcome.rejected_citations, 1);
        let updated = store.memory("m1").unwrap().unwrap();
        assert_eq!(updated.content, "A prefers tea");
        assert_eq!(updated.importance, 7);
        assert_eq!(store.consolidation_cursor("ene").unwrap(), Some("5".into()));
    }
}
