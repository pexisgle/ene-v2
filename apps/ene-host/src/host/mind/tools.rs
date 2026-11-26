//! The parent's explicit memory tools.
use crate::host::{
    agent::{CompanionAgent, WorkAgent},
    background::{self, AbortOnDropHandle},
    db::{
        Result, Store,
        memories::{MindStateChange, MindUpdate, NewMemory, StateUpdate, detail},
    },
    error::HostError,
    mind::embedding::{self, Embeddings},
    model::Credentials,
    resources::HostRuntime,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use ene_host_core::util::now;
use ene_mind::RecallMode;
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
        |called: On<ToolCalled<RecallArgs>>, mut commands: Commands| {
            let (call, agent, args) = (called.call, called.agent, called.args.clone());
            commands.queue(move |world: &mut World| match recall(world, agent, args) {
                Ok(RecallWork::Running(task)) => {
                    world.entity_mut(call).insert(PendingRecall(Some(task)));
                }
                Ok(RecallWork::Ready(text)) => crate::host::permission::answer(world, call, &text),
                Err(error) => crate::host::permission::refuse(world, call, &error.to_string()),
            });
        },
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
         The host binds the scope and commits everything atomically.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| update(world, agent, args)),
    )
    .add_systems(Update, complete_recall.after(AgentLoop));
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))
}

/// Companion owner for tools available to both the companion head and its work hands
/// (`mind_recall` and `mind_read`, `F06`).
fn companion_of_any(world: &World, agent: Entity) -> Result<String> {
    if let Some(agent) = world.get::<CompanionAgent>(agent) {
        return Ok(agent.companion_id.clone());
    }
    if let Some(work) = world.get::<WorkAgent>(agent) {
        return Ok(work.companion_id.clone());
    }
    Err(HostError::Internal(
        ("this tool belongs to a companion or work agent").into(),
    ))
}

enum RecallWork {
    Ready(String),
    Running(AbortOnDropHandle<Result<String>>),
}

#[derive(Component)]
struct PendingRecall(Option<AbortOnDropHandle<Result<String>>>);

fn complete_recall(mut commands: Commands, mut pending: Query<(Entity, &mut PendingRecall)>) {
    for (call, mut pending) in &mut pending {
        if !pending
            .0
            .as_ref()
            .is_some_and(AbortOnDropHandle::is_finished)
        {
            continue;
        }
        let result = pending
            .0
            .take()
            .and_then(background::take)
            .unwrap_or_else(|| {
                Err(HostError::Unavailable(
                    ("memory recall was cancelled").into(),
                ))
            });
        commands.queue(move |world: &mut World| {
            if world.get_entity(call).is_err() {
                return;
            }
            world.entity_mut(call).remove::<PendingRecall>();
            match result {
                Ok(text) => crate::host::permission::answer(world, call, &text),
                Err(error) => {
                    if error.is_storage() {
                        crate::host::fail(world, error.to_string());
                    }
                    crate::host::permission::refuse(world, call, &error.to_string());
                }
            }
        });
    }
}

fn recall(world: &mut World, agent: Entity, args: RecallArgs) -> Result<RecallWork> {
    let companion = companion_of_any(world, agent)?;
    if args.query.chars().count() > 4_000 {
        return Err(HostError::Invalid(("query is too long").into()));
    }
    let store = world.resource::<Store>().clone();
    let embeddings = world.resource::<Embeddings>().clone();
    let credentials = world.get_resource::<Credentials>().cloned();
    let Some(runtime) = world.get_resource::<HostRuntime>() else {
        return recall_result(&store, &companion, &args, &[]).map(RecallWork::Ready);
    };
    Ok(RecallWork::Running(background::spawn(
        &runtime.0,
        world.get_resource::<Wake>(),
        async move {
            let semantic = embedding::semantic_ids(
                &store,
                &embeddings,
                credentials.as_ref(),
                &companion,
                &args.query,
            )
            .await;
            recall_result(&store, &companion, &args, &semantic)
        },
    )))
}

fn recall_result(
    store: &Store,
    companion: &str,
    args: &RecallArgs,
    semantic: &[String],
) -> Result<String> {
    let limit = args.limit.unwrap_or(8).clamp(1, 16) as usize;
    let outcome = store.memory_repo().recall_hybrid(
        companion,
        &args.query,
        RecallMode::Explicit,
        semantic,
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
    let companion = companion_of_any(world, agent)?;
    let store = world.resource::<Store>().clone();
    let memory = store
        .memory_repo()
        .memory(&args.id)?
        .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
    if !memory.visible_to(&companion) {
        return Err(HostError::Permission(
            ("this memory belongs to another companion").into(),
        ));
    }
    // Reading with the tool is an explicit use, so the recall recency moves.
    store
        .memory_repo()
        .cite_memory(&args.id, &format!("{companion}:{}", now()), false)?;
    if memory.scope != companion {
        // A shared memory exposes the facts only: its private origin, emotion
        // and relations stay with the companion that recorded it
        // (docs/design/mind.md §3.3).
        return Ok(json!({
            "id": memory.id,
            "content": memory.content,
            "importance": memory.importance,
            "occurred_at": memory.occurred_at,
            "shared": true,
        })
        .to_string());
    }
    let relations = store.memory_repo().memory_edges(&args.id)?;
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
    let hash = store.memory_repo().mind_hash(&request);
    let state = match &args.state {
        Some(patch) => {
            let current = store
                .companion_repo()
                .companion(&companion)?
                .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
            let mut next = current.state.clone();
            patch.apply(&mut next);
            Some(MindStateChange {
                expected_revision: current.state_revision,
                state: serde_json::to_string(&next)
                    .map_err(|error| HostError::Internal(format!("companion state: {error}")))?,
            })
        }
        None => None,
    };
    let result = store.memory_repo().apply_mind_update(&MindUpdate {
        operation_id: args.operation_id,
        companion_id: companion.clone(),
        request_hash: hash,
        memories: args.memories,
        state,
    })?;
    super::notify_mind(world, &companion);
    Ok(result.to_string())
}
