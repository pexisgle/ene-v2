//! The parent's explicit memory tools.
use crate::host::{
    agent::CompanionAgent,
    db::{
        Result, Store,
        memories::{MindStateChange, MindUpdate, NewMemory, StateUpdate, detail},
        now,
    },
    error::HostError,
    mind::{
        embedding::{self, CompactVectorIndex},
        memory::RecallMode,
    },
    model::Credentials,
    resources::HostRuntime,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
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
    // The semantic half is optional: without a usable embedding connection,
    // or a runtime to embed on, there is no query vector and recall stays
    // lexical (semantic: false).
    let semantic = {
        let runtime = world
            .get_resource::<HostRuntime>()
            .map(|runtime| runtime.0.clone());
        let credentials = world.get_resource::<Credentials>().cloned();
        let mut index = world.resource_mut::<CompactVectorIndex>();
        embedding::semantic_ids(
            &store,
            runtime.as_ref(),
            &mut index,
            credentials.as_ref(),
            &companion,
            &args.query,
        )
    };
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
    Ok(detail(&memory, &relations).to_string())
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
    let state = match &args.state {
        Some(patch) => {
            let current = store
                .companion(&companion)?
                .ok_or_else(|| HostError::not_found("companion not found"))?;
            let mut next = current.state.clone();
            patch.apply(&mut next);
            Some(MindStateChange {
                expected_revision: current.state_revision,
                state: serde_json::to_string(&next)?,
            })
        }
        None => None,
    };
    let result = store.apply_mind_update(&MindUpdate {
        operation_id: args.operation_id,
        companion_id: companion.clone(),
        request_hash: hash,
        memories: args.memories,
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
