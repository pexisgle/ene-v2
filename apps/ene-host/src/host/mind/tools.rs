//! Parent-only Rig open tools (`mind_recall`, `mind_read`, `mind_update`).
use super::{
    ACQUISITIONS, MAX_CONTENT, MAX_ITEMS, MAX_TEXT, MemoryRecord, NORMAL, PRIVATE, SHARED,
    companion, deliver_state, mind_view, notify_mind,
};
use crate::host::{
    Host,
    db::{Result, aborted, digest, invalid, next_revision, not_found, now, validate_id},
    kernel,
};
use bevy_ecs::prelude::*;
use connectrpc::ConnectError;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecallArgs {
    query: String,
    #[serde(default)]
    include_forgotten: bool,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    expected_revision: u64,
    #[serde(default)]
    memories: Vec<NewMemory>,
    #[serde(default)]
    state: Option<StateUpdate>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NewMemory {
    content: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    appraisal: String,
    acquisition: String,
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    occurred_at: Option<String>,
    #[serde(default)]
    audience: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StateUpdate {
    #[serde(default)]
    emotion: Option<Value>,
    #[serde(default)]
    relationships: Option<Vec<Value>>,
    #[serde(default)]
    tendencies: Option<Vec<String>>,
    #[serde(default)]
    concerns: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
struct OperationRecord {
    hash: String,
    response: Value,
}

/// A tool whose calls change nothing: the kernel may run them side by side.
fn read_only() -> ToolOptions {
    ToolOptions {
        footprint: Some(Touches::Reads(Scope::Anything)),
    }
}

pub(crate) fn register_tools(app: &mut App) {
    app.add_open_tool::<RecallArgs, _>(
        "mind_recall",
        "Search the companion's memories. Use it when the current context is not enough.",
        read_only(),
        kernel::run_tool(|world, agent, _, args| recall(world, agent, &args)),
    )
    .add_open_tool::<ReadArgs, _>(
        "mind_read",
        "Read one memory with its provenance.",
        read_only(),
        kernel::run_tool(|world, agent, _, args| read(world, agent, &args)),
    )
    .add_open_tool::<UpdateArgs, _>(
        "mind_update",
        "Add or correct memories and update the current mind. The host binds the owner and input source.",
        ToolOptions::default(),
        kernel::run_tool(|world, agent, call, args| update(world, agent, call, &args)),
    );
}

fn recall(world: &mut World, agent: Entity, args: &RecallArgs) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    if args.query.chars().count() > 4000 {
        return Err(invalid("query is too long"));
    }
    let limit = args.limit.unwrap_or(8).clamp(1, 16) as usize;
    let items: Vec<Value> = world
        .resource::<Host>()
        .db
        .search_memories(
            owner.companion(),
            &args.query,
            args.include_forgotten,
            limit,
        )?
        .iter()
        .map(|memory| memory.view_for(owner.companion()))
        .collect();
    Ok(json!({
        "items": items,
        "lexical": true,
        "semantic": false,
        "pending": false,
        "reason": Value::Null,
    })
    .to_string())
}

fn read(world: &mut World, agent: Entity, args: &ReadArgs) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    validate_id(&args.id)?;
    let memory = world
        .resource::<Host>()
        .db
        .get_memory(&args.id)?
        .filter(|m| m.visible_to(owner.companion()))
        .ok_or_else(|| not_found("memory not found"))?;
    Ok(memory.view_for(owner.companion()).to_string())
}

fn update(world: &mut World, agent: Entity, call: &str, args: &UpdateArgs) -> Result<String> {
    let owner = kernel::parent_owner(world, agent)?;
    validate_mind_update(args)?;
    let companion_id = owner.companion().to_owned();
    let operation_id = format!("{companion_id}:{call}");
    let hash = digest(
        serde_json::to_string(args)
            .map_err(|_| invalid("invalid update"))?
            .as_bytes(),
    );
    let op_key = format!("memory-op:{companion_id}:{operation_id}");
    let db = &world.resource::<Host>().db;
    if let Some(previous) = db.get::<OperationRecord>(&op_key)? {
        if previous.hash == hash {
            return Ok(previous.response.to_string());
        }
        return Err(aborted("operation id reused with different content"));
    }
    let mut record = companion(db, &companion_id)?;
    if record.state_revision != args.expected_revision {
        return Err(ConnectError::aborted(format!(
            "mind revision changed; current revision is {}",
            record.state_revision
        )));
    }
    for id in args
        .memories
        .iter()
        .flat_map(|m| m.supersedes.iter().chain(&m.sources))
    {
        if db.get_memory(id)?.is_none_or(|m| m.owner != companion_id) {
            return Err(aborted("memory belongs to another companion"));
        }
    }
    let input = world
        .get::<Conversation>(agent)
        .and_then(|c| {
            c.turns()
                .next_back()
                .map(|(_, turn)| kernel::input_of(c, turn.start))
        })
        .unwrap_or_default();
    let now = now();
    let inserted: Vec<MemoryRecord> = args
        .memories
        .iter()
        .map(|memory| MemoryRecord {
            id: Uuid::new_v4().to_string(),
            owner: companion_id.clone(),
            kind: "experience".into(),
            audience: memory.audience.clone().unwrap_or_else(|| PRIVATE.into()),
            content: memory.content.clone(),
            description: memory.description.clone(),
            appraisal: memory.appraisal.clone(),
            acquisition: memory.acquisition.clone(),
            sources: if input.is_empty() {
                Vec::new()
            } else {
                vec![json!({"kind": "input", "id": input})]
            },
            recorded_at: now.clone(),
            occurred_at: memory.occurred_at.clone(),
            supersedes: memory.supersedes.clone(),
            recall_preference: NORMAL.into(),
            created_by: "parent".into(),
            revision: 1,
            updated_at: now.clone(),
        })
        .collect();
    let mut state_changed = false;
    if let Some(update) = &args.state {
        if let Some(emotion) = &update.emotion {
            record.state.emotion.clone_from(emotion);
            state_changed = true;
        }
        if let Some(relationships) = &update.relationships {
            record.state.relationships.clone_from(relationships);
            state_changed = true;
        }
        if let Some(tendencies) = &update.tendencies {
            record.state.tendencies.clone_from(tendencies);
            state_changed = true;
        }
        if let Some(concerns) = &update.concerns {
            record.state.concerns.clone_from(concerns);
            state_changed = true;
        }
    }
    let changed = state_changed || !inserted.is_empty();
    if state_changed {
        record.state_revision = next_revision(record.state_revision)?;
    }
    if !inserted.is_empty() {
        record.memory_revision = next_revision(record.memory_revision)?;
    }
    record.companion.updated_at = now;
    record.companion.revision = next_revision(record.companion.revision)?;
    let response = json!({
        "operation_id": operation_id,
        "revision": record.state_revision,
        "changed": changed,
        "memories": inserted.iter().map(|m| json!({"id": m.id, "supersedes": m.supersedes})).collect::<Vec<_>>(),
    });
    let operation = OperationRecord {
        hash,
        response: response.clone(),
    };
    world.resource::<Host>().db.transaction(|tx| {
        for memory in &inserted {
            tx.put_memory(memory)?;
        }
        tx.put(&format!("companion:{}", record.companion.id), &record)?;
        tx.put(&op_key, &operation)?;
        Ok(())
    })?;
    let view = mind_view(&world.resource::<Host>().db, &companion_id)?;
    notify_mind(world, &companion_id, view);
    deliver_state(world, agent, &companion_id)?;
    Ok(response.to_string())
}

fn validate_mind_update(args: &UpdateArgs) -> Result<()> {
    if args.memories.len() > MAX_ITEMS {
        return Err(invalid("too many memories"));
    }
    for memory in &args.memories {
        if memory.content.trim().is_empty() || memory.content.chars().count() > MAX_CONTENT {
            return Err(invalid("memory content is too long"));
        }
        if memory.description.chars().count() > MAX_TEXT
            || memory.appraisal.chars().count() > MAX_TEXT
        {
            return Err(invalid("memory text is too long"));
        }
        if !ACQUISITIONS.contains(&memory.acquisition.as_str()) {
            return Err(invalid("invalid acquisition"));
        }
        if memory.supersedes.len() > MAX_ITEMS || memory.sources.len() > MAX_ITEMS {
            return Err(invalid("too many memory references"));
        }
        if memory
            .audience
            .as_deref()
            .is_some_and(|a| a != PRIVATE && a != SHARED)
        {
            return Err(invalid("invalid audience"));
        }
    }
    Ok(())
}
