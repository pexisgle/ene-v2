//! Bounded post-turn incremental memory consolidation.
use super::{
    ACQUISITIONS, CompanionRecord, MAX_CONTENT, MAX_ITEMS, MAX_TEXT, MemoryRecord, NORMAL, PRIVATE,
    companion, mind_view,
};
use crate::host::{
    Host, Owner, chat,
    db::{Result, aborted, next_revision, now},
    model,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::CompletionRequest;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

const METHOD: &str = "consolidation-v1";
const BATCH_ENTRIES: usize = 12;
const BATCH_CHARS: usize = 8_000;
/// How many times one batch may fail before it is skipped.
///
/// The cursor does not move past a batch that failed, so without this a
/// reply the model never gets right would stop the companion's memory for
/// good: the same entries would be retried on every turn.
const MAX_ATTEMPTS: u64 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Consolidation {
    pub(crate) last_sequence: u64,
    pub(crate) last_offset: u64,
    pub(crate) method_revision: String,
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) jobs: u64,
    pub(crate) model_calls: u64,
    pub(crate) input_chars: u64,
    pub(crate) skipped: u64,
    /// Failed attempts at the batch the cursor is on, reset by a success.
    #[serde(default)]
    pub(crate) attempts: u64,
    pub(crate) updated_at: String,
}

impl Default for Consolidation {
    fn default() -> Self {
        Self {
            last_sequence: 0,
            last_offset: 0,
            method_revision: String::new(),
            state: "unconfigured".into(),
            reason: None,
            jobs: 0,
            model_calls: 0,
            input_chars: 0,
            skipped: 0,
            attempts: 0,
            updated_at: now(),
        }
    }
}

/// What the model is asked for, as a JSON Schema the provider enforces rather
/// than something the prompt has to describe and the reply has to imitate.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Extraction {
    reason: String,
    #[schemars(required)]
    memories: Option<Vec<Extracted>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Extracted {
    content: String,
    #[schemars(required)]
    description: Option<String>,
    #[schemars(required)]
    appraisal: Option<String>,
    acquisition: String,
}

#[derive(Component, Clone)]
pub(crate) struct Batch {
    companion: String,
    through: u64,
    chars: u64,
    memory_revision: u64,
    prompt: String,
    sources: Vec<Value>,
}

#[derive(Component)]
pub(crate) struct Applied;

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let companions = world
        .resource::<Host>()
        .db
        .list::<CompanionRecord>("companion:")?;
    let mut host = world.resource_mut::<Host>();
    for (_, record) in companions {
        host.consolidation_queue.insert(record.companion.id);
    }
    Ok(())
}

pub(crate) fn start_consolidation(
    mut commands: Commands,
    mut host: ResMut<Host>,
    agents: Query<(Entity, &Owner, &Conversation)>,
    pending: Query<(&Batch, &ForAgent)>,
) {
    if !host.consolidate || host.consolidation_queue.is_empty() {
        return;
    }
    let pending: HashSet<Entity> = pending.iter().map(|(_, agent)| agent.0).collect();
    let queue: Vec<String> = host.consolidation_queue.drain().collect();
    for companion_id in queue {
        // A companion's work agents share its id; only the parent is read.
        let Some((agent, _, conversation)) = agents
            .iter()
            .find(|(_, owner, _)| owner.companion() == companion_id && !owner.is_work())
        else {
            continue;
        };
        if pending.contains(&agent) {
            host.consolidation_queue.insert(companion_id);
            continue;
        }
        let cursor = consolidation_cursor(&host, &companion_id);
        let Some(batch) = collect_batch(&host, conversation, &companion_id, &cursor) else {
            continue;
        };
        let models = host.models.clone();
        let (connection, binding) =
            match models.connect(&host, &companion_id, model::Purpose::Memory) {
                Ok(pair) => pair,
                Err(error) => {
                    // Not the batch's fault: a missing model must not count
                    // against it, or it would be skipped once one is set up.
                    record_failure(&mut host, &companion_id, &cursor, &error.to_string(), None);
                    continue;
                }
            };
        let request = CompletionRequest::new(batch.prompt.clone())
            .output_schema(schemars::schema_for!(Extraction));
        commands.spawn((
            ModelRequest::new(request).on(connection),
            ForAgent(agent),
            batch,
            binding,
        ));
    }
}

fn collect_batch(
    host: &Host,
    conversation: &Conversation,
    companion_id: &str,
    cursor: &Consolidation,
) -> Option<Batch> {
    let turns: Vec<_> = conversation.turns().collect();
    let mut lines = Vec::new();
    let mut sources = Vec::new();
    let mut chars = 0usize;
    let mut through = cursor.last_sequence;
    for (index, (_, turn)) in turns.iter().enumerate() {
        if !matches!(turn.end, Some(TurnEnd::Answered { .. })) {
            continue;
        }
        let end = turns.get(index + 1).map(|(_, next)| next.start);
        for entry in conversation.entries() {
            if entry.id < turn.start
                || end.is_some_and(|end| entry.id >= end)
                || entry.id.get() <= cursor.last_sequence
            {
                continue;
            }
            // Only the user's own words, the model's replies and a work
            // agent's report are evidence. Injected context (recall notes,
            // the mind state) is the host's own text, not something to
            // learn from again.
            let eligible = entry.is_users_own()
                || (matches!(entry.message, message::Message::Assistant(_))
                    && entry.author.is_none())
                || entry
                    .author
                    .is_some_and(|author| author.kind == AuthorKind::Agent);
            if !eligible {
                continue;
            }
            let Some(text) = chat::entry_text(entry.message) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            through = through.max(entry.id.get());
            chars = chars.saturating_add(text.chars().count());
            sources.push(json!({"kind": "input", "id": entry.id.get().to_string()}));
            let text = if entry
                .author
                .is_some_and(|author| author.kind == AuthorKind::Agent)
            {
                format!("[作業報告] {text}")
            } else {
                text
            };
            lines.push(format!("{}: {}", entry.id.get(), text));
            if lines.len() >= BATCH_ENTRIES || chars >= BATCH_CHARS {
                break;
            }
        }
        if lines.len() >= BATCH_ENTRIES || chars >= BATCH_CHARS {
            break;
        }
    }
    if lines.is_empty() {
        if cursor.state != "ready" || cursor.last_sequence != through {
            let mut next = cursor.clone();
            next.last_sequence = through;
            next.state = "ready".into();
            next.updated_at = now();
            let _ = host.db.put(&format!("memory-cursor:{companion_id}"), &next);
        }
        return None;
    }
    let record = companion(&host.db, companion_id).ok()?;
    let state_json = serde_json::to_string(&record.state).unwrap_or_default();
    let prompt = format!(
        "以下は保存済みの会話です。ユーザーの好み・思い出・合意・有用な手順のうち、今後も覚えておく価値のあるものだけを抽出してください。\n\
         作者の人格や会話例は実体験ではありません。事実と解釈を区別し、根拠のない断定はしないでください。\n\
         各記憶の acquisition は、ユーザーが述べたことは user_statement、アシスタント自身の体験は direct_experience、\
         作業報告は report、共有された記憶は shared、推測は inference にしてください。\n\
         覚えることがなければ memories を空にしてください。\n\
         現在の心: {state_json}\n\
         会話:\n{}",
        lines.join("\n")
    );
    Some(Batch {
        companion: companion_id.into(),
        through,
        chars: chars as u64,
        memory_revision: record.memory_revision,
        prompt,
        sources,
    })
}

#[allow(clippy::type_complexity)]
pub(crate) fn apply_consolidation(
    mut calls: Query<(Entity, &Batch, &Outcome), (ReadyToSettle, Without<Applied>)>,
    verdicts: Query<&RetryVerdict>,
    mut commands: Commands,
    mut host: ResMut<Host>,
) {
    for (call, batch, outcome) in &mut calls {
        if let Some(response) = outcome.completion() {
            if let Err(error) = apply_batch(&host, batch, &response.text()) {
                let cursor = consolidation_cursor(&host, &batch.companion);
                record_failure(
                    &mut host,
                    &batch.companion,
                    &cursor,
                    &error.to_string(),
                    Some(batch.through),
                );
            } else if let Ok(view) = mind_view(&host.db, &batch.companion) {
                host.notify(
                    Some(batch.companion.clone()),
                    None,
                    w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
                        mind: view.into(),
                        ..Default::default()
                    })),
                );
            }
            commands.entity(call).insert(Applied);
        } else if let Ok(verdict) = verdicts.get(call)
            && matches!(verdict.0, Verdict::GaveUp(_) | Verdict::Final)
        {
            let cursor = consolidation_cursor(&host, &batch.companion);
            record_failure(
                &mut host,
                &batch.companion,
                &cursor,
                "generation failed",
                Some(batch.through),
            );
            commands.entity(call).insert(Applied);
        }
    }
}

fn consolidation_cursor(host: &Host, companion_id: &str) -> Consolidation {
    host.db
        .get::<Consolidation>(&format!("memory-cursor:{companion_id}"))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The reply without the code fence a model sometimes adds around JSON.
fn strip_fence(text: &str) -> &str {
    let text = text.trim();
    let text = match text.strip_prefix("```") {
        Some(rest) => rest.split_once('\n').map_or(rest, |(_, body)| body),
        None => text,
    };
    text.trim_end()
        .strip_suffix("```")
        .map_or(text, str::trim)
        .trim()
}

fn apply_batch(host: &Host, batch: &Batch, text: &str) -> Result<()> {
    let extraction: Extraction = serde_json::from_str(strip_fence(text))
        .map_err(|_| aborted("invalid consolidation output"))?;
    let reason = extraction.reason.trim();
    if reason.is_empty() {
        return Err(aborted("invalid consolidation output"));
    }
    let extracted = extraction.memories.unwrap_or_default();
    if extracted.len() > MAX_ITEMS {
        return Err(aborted("too many consolidated memories"));
    }
    let mut current = companion(&host.db, &batch.companion)?;
    if current.memory_revision != batch.memory_revision {
        return Err(aborted("memory revision changed"));
    }
    let now = now();
    let memories: Vec<MemoryRecord> = extracted
        .iter()
        .filter_map(|item| {
            if item.content.trim().is_empty() || item.content.chars().count() > MAX_CONTENT {
                return None;
            }
            if !ACQUISITIONS.contains(&item.acquisition.as_str()) {
                return None;
            }
            Some(MemoryRecord {
                id: Uuid::new_v4().to_string(),
                owner: batch.companion.clone(),
                kind: "experience".into(),
                audience: PRIVATE.into(),
                content: item.content.clone(),
                description: item
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(MAX_TEXT)
                    .collect(),
                appraisal: item
                    .appraisal
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(MAX_TEXT)
                    .collect(),
                acquisition: item.acquisition.clone(),
                sources: batch.sources.clone(),
                recorded_at: now.clone(),
                occurred_at: None,
                supersedes: Vec::new(),
                recall_preference: NORMAL.into(),
                created_by: "consolidation".into(),
                revision: 1,
                updated_at: now.clone(),
            })
        })
        .collect();
    if !memories.is_empty() {
        current.memory_revision = next_revision(current.memory_revision)?;
        current.companion.revision = next_revision(current.companion.revision)?;
        current.companion.updated_at.clone_from(&now);
    }
    let mut next = consolidation_cursor(host, &batch.companion);
    next.last_sequence = batch.through;
    next.last_offset = 0;
    next.method_revision = METHOD.into();
    next.state = "ready".into();
    next.reason = Some(reason.chars().take(500).collect());
    next.jobs += 1;
    next.model_calls += 1;
    next.input_chars += batch.chars;
    next.attempts = 0;
    next.updated_at = now;
    host.db.transaction(|tx| {
        for memory in &memories {
            tx.put_memory(memory)?;
        }
        if !memories.is_empty() {
            tx.put(&format!("companion:{}", batch.companion), &current)?;
        }
        tx.put(&format!("memory-cursor:{}", batch.companion), &next)?;
        Ok(())
    })
}

/// Records a failed attempt and returns the cursor to try again.
///
/// `through` is the batch that failed, `None` when the run did not get to one
/// (no model): a batch that fails `MAX_ATTEMPTS` times is skipped, so one
/// bad reply cannot hold the companion's memory forever.
fn record_failure(
    host: &mut Host,
    companion_id: &str,
    cursor: &Consolidation,
    reason: &str,
    through: Option<u64>,
) {
    let mut next = cursor.clone();
    next.state = "error".into();
    next.reason = Some(crate::truncate_chars(reason, 500));
    if let Some(through) = through {
        next.attempts = next.attempts.saturating_add(1);
        if next.attempts >= MAX_ATTEMPTS {
            next.last_sequence = next.last_sequence.max(through);
            next.last_offset = 0;
            next.attempts = 0;
            next.skipped = next.skipped.saturating_add(1);
            next.reason = Some(crate::truncate_chars(
                &format!("skipped after {MAX_ATTEMPTS} failed attempts: {reason}"),
                500,
            ));
        }
    }
    next.updated_at = now();
    let _ = host.db.put(&format!("memory-cursor:{companion_id}"), &next);
    tracing::warn!(companion = companion_id, reason, "consolidation failed");
    super::super::notice(host, companion_id, "consolidation_failed", reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strict structured output takes only schemas that name every property
    /// as required and allow no others, at every level. A field with a
    /// default would fail the request rather than the parse.
    #[test]
    fn the_extraction_schema_is_strict() {
        fn assert_strict(schema: &Value) {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for name in properties.keys() {
                    assert!(required.contains(&name.as_str()), "{name} must be required");
                }
                assert!(
                    schema.get("additionalProperties") == Some(&Value::Bool(false)),
                    "every object must refuse unknown fields"
                );
            }
            let defs = schema.get("$defs").and_then(Value::as_object);
            for value in defs.into_iter().flat_map(|defs| defs.values()) {
                assert_strict(value);
            }
        }
        let schema = serde_json::to_value(schemars::schema_for!(Extraction)).unwrap();
        assert_strict(&schema);
    }
}
