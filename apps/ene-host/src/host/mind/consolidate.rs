//! Bounded post-turn incremental memory consolidation.
use super::{MAX_CONTENT, MAX_ITEMS, MAX_TEXT, mind_view};
use crate::host::{
    agent::AgentKey,
    chat,
    consolidation::Consolidation,
    db::{Result, Store, next_revision, now},
    error::HostError,
    memory::{Acquisition, Memory},
    model::{self},
    resources::Models,
    resources::{CompactionQueue, ConsolidationQueue, HostOptions},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::CompletionRequest;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

const METHOD: &str = "consolidation-v1";
const BATCH_ENTRIES: usize = 12;
const BATCH_CHARS: usize = 8_000;
const MAX_ATTEMPTS: u64 = 3;

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
    importance: u8,
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

#[derive(Component, Clone)]
pub(crate) struct ConsolidationJob {
    pub(crate) agent: Entity,
}

#[derive(Component)]
pub(crate) struct Applied;

pub(crate) fn start(
    mut commands: Commands,
    store: Res<Store>,
    models: Res<Models>,
    options: Res<HostOptions>,
    mut queue: ResMut<ConsolidationQueue>,
    agents: Query<(Entity, &AgentKey, &Conversation)>,
    pending: Query<&ConsolidationJob, Without<Applied>>,
) {
    if !options.consolidate || queue.0.is_empty() {
        return;
    }
    let pending: HashSet<Entity> = pending.iter().map(|job| job.agent).collect();
    let queued: Vec<String> = queue.0.drain().collect();
    for companion_id in queued {
        let Some((agent, _, conversation)) = agents
            .iter()
            .find(|(_, key, _)| !key.is_work() && key.companion() == companion_id)
        else {
            continue;
        };
        if pending.contains(&agent) {
            queue.0.insert(companion_id);
            continue;
        }
        let cursor = store
            .consolidation(&companion_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let Some(batch) = collect_batch(&store, conversation, &companion_id, &cursor) else {
            continue;
        };
        let (connection, binding) =
            match models
                .0
                .connect(&store, &companion_id, model::Purpose::Consolidation)
            {
                Ok(pair) => pair,
                Err(error) => {
                    record_failure(
                        &mut commands,
                        &store,
                        &companion_id,
                        &cursor,
                        &error.to_string(),
                        None,
                    );
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
            ConsolidationJob { agent },
        ));
    }
}

fn collect_batch(
    store: &Store,
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
            let _ = store.save_consolidation(companion_id, &next);
        }
        return None;
    }
    let record = store.companion(companion_id).ok().flatten()?;
    let state_json = serde_json::to_string(&record.state).unwrap_or_default();
    let prompt = format!(
        "以下は保存済みの会話です。ユーザーの好み・思い出・合意・有用な手順のうち、今後も覚えておく価値のあるものだけを抽出してください。\n\
         作者の人格や会話例は実体験ではありません。事実と解釈を区別し、根拠のない断定はしないでください。\n\
         各記憶の acquisition は、ユーザーが述べたことは user_statement、アシスタント自身の体験は direct_experience、\
         作業報告は report、共有された記憶は shared、推測は inference にしてください。\n\
         importance は 1〜10 の整数で、日常の雑談は1〜3、継続的な趣味・仕事は4〜6、約束・大きな決定は7〜10 にしてください。\n\
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
pub(crate) fn apply(
    mut commands: Commands,
    store: Res<Store>,
    mut queue: ResMut<ConsolidationQueue>,
    mut compaction: ResMut<CompactionQueue>,
    mut calls: Query<(Entity, &Batch, &Outcome), (ReadyToSettle, Without<Applied>)>,
    verdicts: Query<&RetryVerdict>,
    mut notifier: ResMut<crate::host::notify::Notifier>,
) {
    for (call, batch, outcome) in &mut calls {
        if let Some(response) = outcome.completion() {
            match apply_batch(&store, batch, &response.text()) {
                Ok(Some(view)) => {
                    notifier.push(
                        Some(batch.companion.clone()),
                        None,
                        w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
                            companion_id: batch.companion.clone(),
                            profile_revision: view
                                .profile
                                .as_option()
                                .map_or(0, |profile| profile.revision),
                            state_revision: view
                                .state
                                .as_option()
                                .map_or(0, |state| state.revision),
                            updated_at: view
                                .state
                                .as_option()
                                .map_or_else(String::new, |state| state.updated_at.clone()),
                            ..Default::default()
                        })),
                    );
                    queue.0.insert(batch.companion.clone());
                    compaction.0.insert(batch.companion.clone());
                }
                Ok(None) => {
                    queue.0.insert(batch.companion.clone());
                }
                Err(error) => {
                    let cursor = store
                        .consolidation(&batch.companion)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    record_failure(
                        &mut commands,
                        &store,
                        &batch.companion,
                        &cursor,
                        &error.to_string(),
                        Some(batch.through),
                    );
                }
            }
            commands.entity(call).insert(Applied);
        } else if let Ok(verdict) = verdicts.get(call)
            && matches!(verdict.0, Verdict::GaveUp(_) | Verdict::Final)
        {
            let cursor = store
                .consolidation(&batch.companion)
                .ok()
                .flatten()
                .unwrap_or_default();
            record_failure(
                &mut commands,
                &store,
                &batch.companion,
                &cursor,
                "generation failed",
                Some(batch.through),
            );
            commands.entity(call).insert(Applied);
        }
    }
}

fn apply_batch(store: &Store, batch: &Batch, text: &str) -> Result<Option<w::MindView>> {
    let extraction: Extraction = serde_json::from_str(strip_fence(text))
        .map_err(|_| HostError::conflict("invalid consolidation output"))?;
    let reason = extraction.reason.trim();
    if reason.is_empty() {
        return Err(HostError::conflict("invalid consolidation output"));
    }
    let extracted = extraction.memories.unwrap_or_default();
    if extracted.len() > MAX_ITEMS {
        return Err(HostError::conflict("too many consolidated memories"));
    }
    let mut current = store
        .companion(&batch.companion)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    if current.memory_revision != batch.memory_revision {
        return Err(HostError::conflict("memory revision changed"));
    }
    let time = now();
    let memories: Vec<Memory> = extracted
        .iter()
        .filter_map(|item| {
            if item.content.trim().is_empty() || item.content.chars().count() > MAX_CONTENT {
                return None;
            }
            let acquisition = Acquisition::parse(&item.acquisition)?;
            Some(Memory {
                id: Uuid::new_v4().to_string(),
                owner: batch.companion.clone(),
                audiences: Vec::new(),
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
                acquisition,
                importance: item.importance.min(10),
                occurred_at: None,
                recorded_at: time.clone(),
                last_accessed_at: None,
                memory_revision: 1,
                content_revision: 1,
                emotion: None,
                source_context: Value::Array(batch.sources.clone()),
                supersedes: Vec::new(),
                created_by: "consolidation".into(),
                updated_at: time.clone(),
            })
        })
        .collect();
    let mut next = store.consolidation(&batch.companion)?.unwrap_or_default();
    if !memories.is_empty() {
        current.memory_revision = next_revision(current.memory_revision)?;
        current.revision = next_revision(current.revision)?;
        current.updated_at = time.clone();
    }
    next.last_sequence = batch.through;
    next.last_offset = 0;
    next.method_revision = METHOD.into();
    next.state = "ready".into();
    next.reason = Some(reason.chars().take(500).collect());
    next.jobs = next.jobs.saturating_add(1);
    next.model_calls = next.model_calls.saturating_add(1);
    next.input_chars = next.input_chars.saturating_add(batch.chars);
    next.attempts = 0;
    next.updated_at = time;
    store.apply_consolidation(batch.memory_revision, &current, &memories, &next)?;
    Ok(Some(mind_view(store, &batch.companion)?))
}

fn record_failure(
    commands: &mut Commands,
    store: &Store,
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
    let _ = store.save_consolidation(companion_id, &next);
    let companion = companion_id.to_owned();
    let reason = crate::truncate_chars(reason, 500);
    commands.queue(move |world: &mut World| {
        if let Some(mut notifier) = world.get_resource_mut::<crate::host::notify::Notifier>() {
            notifier.notice(&companion, "consolidation_failed", &reason);
        }
    });
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
