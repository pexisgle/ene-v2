//! Parent-only Rig open tools for explicit memory use.
use super::{MAX_CONTENT, MAX_ITEMS, MAX_TEXT, mind_view, notify_mind};
use crate::host::{
    agent::AgentKey,
    chat,
    db::{Result, Store, digest, next_revision, now, validate_id},
    error::HostError,
    memory::{Acquisition, Memory, MindOperation, RecallMode, SHARED_AUDIENCE},
    tools,
};
use bevy_ecs::prelude::*;
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
    importance: Option<u8>,
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
        tools::run_tool(|world, agent, _, args| recall(world, agent, args)),
    )
    .add_open_tool::<ReadArgs, _>(
        "mind_read",
        "Read one memory with its provenance.",
        read_only(),
        tools::run_tool(|world, agent, _, args| read(world, agent, args)),
    )
    .add_open_tool::<UpdateArgs, _>(
        "mind_update",
        "Add or correct memories and update the current mind. The host binds the owner and input source.",
        ToolOptions::default(),
        tools::run_tool(update),
    );
}

fn recall(world: &mut World, agent: Entity, args: RecallArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    if args.query.chars().count() > 4000 {
        return Err(HostError::invalid("query is too long"));
    }
    let limit = args.limit.unwrap_or(8).clamp(1, 16) as usize;
    let store = world.resource::<Store>().clone();
    let items: Vec<Value> = store
        .recall(key.companion(), &args.query, RecallMode::Explicit, limit)?
        .iter()
        .map(|memory| memory.view_for(key.companion()))
        .collect();
    Ok(json!({
        "items": items,
        "semantic": false,
        "pending": false,
        "reason": Value::Null,
    })
    .to_string())
}

fn read(world: &mut World, agent: Entity, args: ReadArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    validate_id(&args.id)?;
    let store = world.resource::<Store>().clone();
    let memory = store
        .memory(&args.id)?
        .filter(|memory| memory.visible_to(key.companion()))
        .ok_or_else(|| HostError::not_found("memory not found"))?;
    store.touch_memory(&args.id, &now())?;
    Ok(memory.view_for(key.companion()).to_string())
}

fn update(world: &mut World, agent: Entity, call: &str, args: UpdateArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    validate_update(&args)?;
    let companion_id = key.companion().to_owned();
    let operation_id = format!("{companion_id}:{call}");
    let hash = digest(
        serde_json::to_string(&args)
            .map_err(|_| HostError::invalid("invalid update"))?
            .as_bytes(),
    );
    let store = world.resource::<Store>().clone();
    if let Some(previous) = store.mind_operation(&companion_id, &operation_id)? {
        if previous.hash == hash {
            return Ok(previous.response);
        }
        return Err(HostError::conflict(
            "operation id reused with different content",
        ));
    }
    let mut record = store
        .companion(&companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let previous_revision = record.revision;
    if record.state_revision != args.expected_revision {
        return Err(HostError::conflict(format!(
            "mind revision changed; current revision is {}",
            record.state_revision
        )));
    }
    for id in args
        .memories
        .iter()
        .flat_map(|memory| memory.supersedes.iter().chain(&memory.sources))
    {
        if store
            .memory(id)?
            .is_none_or(|memory| memory.owner != companion_id)
        {
            return Err(HostError::conflict("memory belongs to another companion"));
        }
    }
    let input = world.get::<Conversation>(agent).and_then(|conversation| {
        conversation
            .turns()
            .next_back()
            .and_then(|(_, turn)| chat::input_of(conversation, turn.start))
    });
    let time = now();
    let inserted: Vec<Memory> = args
        .memories
        .iter()
        .map(|memory| {
            let audiences = match memory.audience.as_deref() {
                None | Some("private") => Vec::new(),
                Some("shared") => vec![SHARED_AUDIENCE.to_owned()],
                Some(_) => Vec::new(),
            };
            Memory {
                id: Uuid::new_v4().to_string(),
                owner: companion_id.clone(),
                audiences,
                content: memory.content.clone(),
                description: memory.description.clone(),
                appraisal: memory.appraisal.clone(),
                acquisition: Acquisition::parse(&memory.acquisition)
                    .unwrap_or(Acquisition::UserStatement),
                importance: memory.importance.unwrap_or(3).min(10),
                occurred_at: memory.occurred_at.clone(),
                recorded_at: time.clone(),
                last_accessed_at: None,
                memory_revision: 1,
                content_revision: 1,
                emotion: None,
                source_context: json!({
                    "input": input,
                    "sources": memory.sources,
                }),
                supersedes: memory.supersedes.clone(),
                created_by: "parent".into(),
                updated_at: time.clone(),
            }
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
    if changed {
        record.revision = next_revision(record.revision)?;
        record.updated_at = time;
    }
    let response = json!({
        "operation_id": operation_id,
        "revision": record.state_revision,
        "changed": changed,
        "memories": inserted.iter().map(|memory| json!({
            "id": memory.id,
            "supersedes": memory.supersedes,
        })).collect::<Vec<_>>(),
    });
    let operation = MindOperation {
        operation_id,
        hash,
        response: response.to_string(),
    };
    store.apply_mind_update(previous_revision, &record, &inserted, &operation)?;
    let view = mind_view(&store, &companion_id)?;
    notify_mind(world, &companion_id, &view);
    Ok(response.to_string())
}

fn validate_update(args: &UpdateArgs) -> Result<()> {
    if args.memories.len() > MAX_ITEMS {
        return Err(HostError::invalid("too many memories"));
    }
    for memory in &args.memories {
        if memory.content.trim().is_empty() || memory.content.chars().count() > MAX_CONTENT {
            return Err(HostError::invalid("memory content is too long"));
        }
        if memory.description.chars().count() > MAX_TEXT
            || memory.appraisal.chars().count() > MAX_TEXT
        {
            return Err(HostError::invalid("memory text is too long"));
        }
        if Acquisition::parse(&memory.acquisition).is_none() {
            return Err(HostError::invalid("invalid acquisition"));
        }
        if memory.supersedes.len() > MAX_ITEMS || memory.sources.len() > MAX_ITEMS {
            return Err(HostError::invalid("too many memory references"));
        }
        if memory
            .audience
            .as_deref()
            .is_some_and(|audience| audience != "private" && audience != "shared")
        {
            return Err(HostError::invalid("invalid audience"));
        }
        if memory.importance.is_some_and(|importance| importance > 10) {
            return Err(HostError::invalid("invalid importance"));
        }
    }
    Ok(())
}

fn parent_key(world: &mut World, agent: Entity) -> Result<AgentKey> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    if key.is_work() {
        return Err(HostError::permission("not available to work agents"));
    }
    Ok(key)
}
