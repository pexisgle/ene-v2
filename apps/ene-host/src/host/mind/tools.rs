//! The parent's explicit memory tools.
use crate::host::{
    agent::CompanionAgent,
    db::{
        Result, Store,
        memories::{MemoryCorrection, MemoryForget, MemoryWrite, MindStateChange, MindUpdate},
        now,
    },
    error::HostError,
    json::JsonDoc,
    mind::{
        embedding::{self, CompactVectorIndex, Embeddings},
        memory::RecallMode,
    },
    model::Credentials,
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
    /// New memories, corrections and forgets to apply atomically.
    #[serde(default)]
    pub(crate) memories: Vec<NewMemory>,
    /// The current mind to move to (all parts are schema'd documents).
    #[serde(default)]
    pub(crate) state: Option<StateUpdate>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewMemory {
    /// Correct an existing memory instead of recording a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    /// The memory text; a correction may omit it to change only importance.
    #[serde(default)]
    pub(crate) content: String,
    /// 0-10; 10 is injected every turn, 0-1 are effectively forgotten.
    #[serde(default)]
    pub(crate) importance: Option<u8>,
    /// Share with these companions ("*" for every companion).
    #[serde(default)]
    pub(crate) audiences: Vec<String>,
    #[serde(default)]
    pub(crate) occurred_at: Option<String>,
    /// The revision that was read; a stale value is a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_revision: Option<u64>,
    /// An existing memory this record supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supersedes: Option<String>,
    /// Forget the addressed memory by lowering its importance to zero.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) forget: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
        "Record, correct or forget memories and update the current mind. \
         The host binds the owner and commits everything atomically.",
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
    // The semantic half is optional: without a usable embedding connection
    // there is no query vector and recall stays lexical (semantic: false).
    let mut semantic = Vec::new();
    let target = store.companion(&companion)?.and_then(|record| {
        let credentials = world.get_resource::<Credentials>().cloned();
        embedding::embedding_target(&store, credentials.as_ref(), &record)
            .ok()
            .flatten()
    });
    if let Some(target) = &target {
        let query_vector = {
            let embeddings = world.resource::<Embeddings>();
            embeddings
                .source()
                .and_then(|source| embedding::embed_query(source, target, &args.query).ok())
        };
        if let Some(query_vector) = query_vector {
            let mut index = world.resource_mut::<CompactVectorIndex>();
            if index.sync(&store, &target.space).is_ok() {
                semantic = index
                    .ranked(&target.space, &query_vector, 20)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
            }
        }
    }
    let outcome = store.recall_hybrid(
        &companion,
        &args.query,
        RecallMode::Explicit,
        &semantic,
        limit,
    )?;
    let items: Vec<Value> = outcome
        .items
        .iter()
        .map(|memory| {
            json!({
                "id": memory.memory.id,
                "content": memory.memory.content,
                "importance": memory.memory.importance,
                "occurred_at": memory.memory.occurred_at,
                "score": memory.score,
                "relevance": memory.relevance,
            })
        })
        .collect();
    Ok(json!({"items": items, "semantic": outcome.semantic}).to_string())
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
        "operation_id": &args.operation_id,
        "memories": &args.memories,
        "state": &args.state,
    });
    let hash = store.mind_hash(&request);
    let mut writes = Vec::with_capacity(args.memories.len());
    for memory in &args.memories {
        if memory.forget {
            let Some(id) = &memory.id else {
                return Err(HostError::invalid(
                    "forget needs the id of the memory to lower",
                ));
            };
            writes.push(MemoryWrite::Forget(MemoryForget {
                id: id.clone(),
                expected_revision: memory.expected_revision,
            }));
            continue;
        }
        if let Some(id) = &memory.id {
            if memory.content.trim().is_empty()
                && memory.importance.is_none()
                && memory.supersedes.is_none()
            {
                return Err(HostError::invalid(
                    "a correction needs content, importance or supersedes",
                ));
            }
            writes.push(MemoryWrite::Update(MemoryCorrection {
                id: id.clone(),
                expected_revision: memory.expected_revision,
                content: (!memory.content.trim().is_empty()).then(|| memory.content.clone()),
                importance: memory.importance,
                supersedes: memory.supersedes.clone(),
            }));
            continue;
        }
        if memory.content.trim().is_empty() {
            return Err(HostError::invalid("memory content is required"));
        }
        writes.push(MemoryWrite::Insert {
            memory: crate::host::db::memories::NewMemory {
                id: uuid::Uuid::new_v4().to_string(),
                owner: companion.clone(),
                audiences: memory.audiences.clone(),
                content: memory.content.clone(),
                importance: memory.importance.unwrap_or(3).min(10),
                occurred_at: memory.occurred_at.clone(),
                source_context: json!({
                    "source": "mind_update",
                    "operation_id": args.operation_id,
                }),
            },
            supersedes: memory.supersedes.clone(),
        });
    }
    let state = match &args.state {
        Some(patch) => {
            let current = store
                .companion(&companion)?
                .ok_or_else(|| HostError::not_found("companion not found"))?;
            let mut next = current.state.clone();
            if let Some(emotion) = &patch.emotion {
                next.emotion = JsonDoc::new("ene.emotion.v1", emotion.clone());
            }
            if let Some(relationships) = &patch.relationships {
                next.relationships = JsonDoc::new("ene.relationships.v1", relationships.clone());
            }
            if let Some(tendencies) = &patch.tendencies {
                next.tendencies = JsonDoc::new("ene.tendencies.v1", tendencies.clone());
            }
            if let Some(concerns) = &patch.concerns {
                next.concerns = JsonDoc::new("ene.concerns.v1", concerns.clone());
            }
            Some(MindStateChange {
                expected_revision: current.state_revision,
                state: serde_json::to_string(&next)?,
            })
        }
        None => None,
    };
    let result = store.apply_mind_update(&MindUpdate {
        operation_id: args.operation_id.clone(),
        companion_id: companion.clone(),
        request_hash: hash,
        writes,
        state,
    })?;
    super::notify_mind(world, &companion);
    Ok(result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_args_keep_the_old_wire_shape_for_retries() {
        // A plain insert serialises exactly as it did before corrections
        // existed, so an operation_id receipt from the old host still
        // replays instead of conflicting.
        let value = serde_json::to_value(NewMemory {
            id: None,
            content: "tea".into(),
            importance: None,
            audiences: Vec::new(),
            occurred_at: None,
            expected_revision: None,
            supersedes: None,
            forget: false,
        })
        .unwrap();
        assert_eq!(
            value,
            json!({
                "content": "tea",
                "importance": null,
                "audiences": [],
                "occurred_at": null,
            })
        );
    }
}
