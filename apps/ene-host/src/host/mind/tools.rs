//! The parent's explicit memory tools.
use crate::host::{
    agent::CompanionAgent,
    db::{Result, Store, digest, now},
    error::HostError,
    json::JsonDoc,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallArgs {
    /// What to search the memories for.
    pub(crate) query: String,
    /// How many memories to return (1-16).
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    /// The memory id from a recall result.
    pub(crate) id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateArgs {
    /// A stable id for this update; a retry with the same id is idempotent.
    pub(crate) operation_id: String,
    /// New memories to record.
    #[serde(default)]
    pub(crate) memories: Vec<NewMemory>,
    /// The current mind to move to (all parts are schema'd documents).
    #[serde(default)]
    pub(crate) state: Option<StateUpdate>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewMemory {
    pub(crate) content: String,
    /// 0-10; 10 is injected every turn, 0-1 are effectively forgotten.
    #[serde(default)]
    pub(crate) importance: Option<u8>,
    /// Share with these companions ("*" for every companion).
    #[serde(default)]
    pub(crate) audiences: Vec<String>,
    pub(crate) occurred_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateUpdate {
    pub(crate) emotion: Option<Value>,
    pub(crate) relationships: Option<Value>,
    pub(crate) tendencies: Option<Value>,
    pub(crate) concerns: Option<Value>,
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<RecallArgs, _>(
        "mind_recall",
        "Search the companion's memories. Use it when the current context is not enough.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| recall(world, agent, args)),
    )
    .add_open_tool::<ReadArgs, _>(
        "mind_read",
        "Read one memory with its provenance. This counts as using it.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| read(world, agent, args)),
    )
    .add_open_tool::<UpdateArgs, _>(
        "mind_update",
        "Record or correct memories and update the current mind. The host binds the owner.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| update(world, agent, args)),
    );
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::internal("this tool belongs to a companion"))
}

fn recall(world: &mut World, agent: Entity, args: RecallArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    if args.query.chars().count() > 4_000 {
        return Err(HostError::invalid("query is too long"));
    }
    let limit = args.limit.unwrap_or(8).clamp(1, 16) as usize;
    let store = world.resource::<Store>().clone();
    let items: Vec<Value> = store
        .recall(
            &companion,
            &args.query,
            crate::host::mind::memory::RecallMode::Explicit,
            limit,
        )?
        .iter()
        .map(|memory| {
            json!({
                "id": memory.memory.id,
                "content": memory.memory.content,
                "importance": memory.memory.importance,
                "occurred_at": memory.memory.occurred_at,
                "score": memory.score,
            })
        })
        .collect();
    Ok(json!({"items": items, "semantic": false}).to_string())
}

fn read(world: &mut World, agent: Entity, args: ReadArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let memory = store
        .memory(&args.id)?
        .ok_or_else(|| HostError::not_found("memory not found"))?;
    if !memory.visible_to(&companion) {
        return Err(HostError::permission(
            "this memory belongs to another companion",
        ));
    }
    // Reading with the tool is an explicit use, so the recall recency moves.
    store.cite_memory(&args.id, &format!("{companion}:{}", now()), false)?;
    if memory.owner != companion {
        // A shared memory exposes the facts only: its private origin, emotion
        // and relations stay with the owner (docs/design/mind.md §3.3).
        return Ok(json!({
            "id": memory.id,
            "content": memory.content,
            "importance": memory.importance,
            "occurred_at": memory.occurred_at,
            "shared": true,
        })
        .to_string());
    }
    let relations = store.memory_edges(&args.id)?;
    Ok(crate::host::db::memories::detail(&memory, &relations).to_string())
}

fn update(world: &mut World, agent: Entity, args: UpdateArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let request = json!({
        "operation_id": args.operation_id,
        "memories": args.memories,
        "state": args.state,
    });
    let hash = store.mind_hash(&request);
    if let Some(existing) = store.mind_operation_hash(&args.operation_id)? {
        if existing == hash {
            return Ok(store
                .mind_operation(&args.operation_id)?
                .unwrap_or_else(|| json!({"replayed": true}))
                .to_string());
        }
        return Err(HostError::conflict(
            "operation_id was already used with different content",
        ));
    }
    let mut stored = Vec::new();
    for memory in &args.memories {
        let record = store.insert_memory(crate::host::db::memories::NewMemory {
            id: uuid::Uuid::new_v4().to_string(),
            owner: companion.clone(),
            audiences: memory.audiences.clone(),
            content: memory.content.clone(),
            importance: memory.importance.unwrap_or(3).min(10),
            occurred_at: memory.occurred_at.clone(),
            source_context: json!({"source": "mind_update", "operation_id": args.operation_id}),
        })?;
        stored.push(record.id);
    }
    if let Some(state) = &args.state {
        let current = store
            .companion(&companion)?
            .ok_or_else(|| HostError::not_found("companion not found"))?;
        let mut next = current.state.clone();
        if let Some(emotion) = &state.emotion {
            next.emotion = JsonDoc::new("ene.emotion.v1", emotion.clone());
        }
        if let Some(relationships) = &state.relationships {
            next.relationships = JsonDoc::new("ene.relationships.v1", relationships.clone());
        }
        if let Some(tendencies) = &state.tendencies {
            next.tendencies = JsonDoc::new("ene.tendencies.v1", tendencies.clone());
        }
        if let Some(concerns) = &state.concerns {
            next.concerns = JsonDoc::new("ene.concerns.v1", concerns.clone());
        }
        store.update_mind_state(&companion, current.state_revision, &next)?;
    }
    let result = json!({
        "memories": stored,
        "state": args.state.is_some(),
    });
    store.record_mind_operation(&args.operation_id, &companion, &hash, &result)?;
    let _ = digest;
    super::notify_mind(world, &companion);
    Ok(result.to_string())
}
