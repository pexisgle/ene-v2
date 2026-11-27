//! The parent's explicit memory tools.
use crate::host::{
    agent::CompanionAgent,
    background::{self, Job},
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
use ene_mind::{RecallCandidate, RecallMode};
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
            commands.queue(move |world: &mut World| {
                let result = companion(world, agent).and_then(|companion| {
                    recall(
                        &RecallServices {
                            store: world.resource(),
                            embeddings: world.resource(),
                            credentials: world.get_resource(),
                            runtime: world.resource(),
                            wake: world.get_resource(),
                        },
                        companion,
                        args,
                    )
                });
                match result {
                    Ok(task) => {
                        world.entity_mut(call).insert(PendingRecall(task));
                    }
                    Err(error) => crate::host::permission::refuse(world, call, &error.to_string()),
                }
            });
        },
    )
    .add_open_tool::<ReadArgs, _>(
        "mind_read",
        "Read one memory with its provenance. This counts as using it.",
        ToolOptions::default(),
        run_tool(|world, agent, call, args: ReadArgs| {
            let (turn, agent_id) = world
                .get::<CallOf>(call)
                .and_then(|of| world.get::<Turn>(of.0))
                .zip(world.get::<AgentId>(agent))
                .ok_or_else(|| {
                    HostError::Internal("memory read has no conversation turn".into())
                })?;
            let citation = format!("{}:{}", agent_id.0, turn.start.get());
            read(
                world.resource(),
                &companion(world, agent)?,
                &args,
                &citation,
            )
        }),
    )
    .add_open_tool::<UpdateArgs, _>(
        "mind_update",
        "Record, correct or forget memories and update the current mind. \
         For an existing memory, use its ID and copy its revision into expected_revision. \
         The host binds the scope and commits everything atomically.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| {
            update(
                world.resource(),
                world.get_resource(),
                &companion(world, agent)?,
                args,
            )
        }),
    )
    .add_systems(Update, complete_recall.after(AgentLoop));
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))
}

#[derive(Component)]
struct PendingRecall(Job<Result<String>>);

fn complete_recall(mut commands: Commands, mut pending: Query<(Entity, &mut PendingRecall)>) {
    for (call, mut pending) in &mut pending {
        let std::task::Poll::Ready(result) = pending.0.poll() else {
            continue;
        };
        let result = result.unwrap_or_else(|| {
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
                        crate::host::fail(world, &error.to_string());
                    }
                    crate::host::permission::refuse(world, call, &error.to_string());
                }
            }
        });
    }
}

struct RecallServices<'a> {
    store: &'a Store,
    embeddings: &'a Embeddings,
    credentials: Option<&'a Credentials>,
    runtime: &'a HostRuntime,
    wake: Option<&'a Wake>,
}

fn recall(
    services: &RecallServices<'_>,
    companion: String,
    args: RecallArgs,
) -> Result<Job<Result<String>>> {
    if args.query.chars().count() > 4_000 {
        return Err(HostError::Invalid(("query is too long").into()));
    }
    let store = services.store.clone();
    let embeddings = services.embeddings.clone();
    let credentials = services.credentials.cloned();
    Ok(background::spawn(
        &services.runtime.0,
        services.wake,
        async move {
            let semantic = embedding::semantic_candidates(
                &store,
                &embeddings,
                credentials.as_ref(),
                &companion,
                &args.query,
                RecallMode::Explicit,
            )
            .await;
            recall_result(&store, &companion, &args, &semantic)
        },
    ))
}

fn recall_result(
    store: &Store,
    companion: &str,
    args: &RecallArgs,
    semantic: &[RecallCandidate],
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
                "revision": memory.memory.memory_revision,
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

fn read(store: &Store, companion: &str, args: &ReadArgs, turn: &str) -> Result<String> {
    let (memory, relations) = store
        .memory_repo()
        .read_and_cite(companion, &args.id, turn)?;
    if memory.scope != companion {
        // A shared memory exposes the facts only: its private origin, emotion
        // and relations stay with the companion that recorded it
        // (docs/design/mind.md).
        return Ok(json!({
            "id": memory.id,
            "revision": memory.memory_revision,
            "content": memory.content,
            "importance": memory.importance,
            "occurred_at": memory.occurred_at,
            "shared": true,
        })
        .to_string());
    }
    Ok(detail(&memory, &relations).to_string())
}

fn update(
    store: &Store,
    notifier: Option<&crate::host::notify::Notifier>,
    companion: &str,
    args: UpdateArgs,
) -> Result<String> {
    let request = json!({
        "operation_id": &args.operation_id,
        "memories": &args.memories,
        "state": &args.state,
    });
    let hash = store.memory_repo().mind_hash(&request);
    let state = match args.state {
        Some(patch) => {
            let current = store
                .companion_repo()
                .companion(companion)?
                .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
            let mut next = current.state;
            patch.apply(&mut next);
            Some(MindStateChange {
                expected_revision: current.state_revision,
                state: next,
            })
        }
        None => None,
    };
    let result = store.memory_repo().apply_mind_update(&MindUpdate {
        operation_id: args.operation_id,
        companion_id: companion.to_owned(),
        request_hash: hash,
        memories: args.memories,
        state,
    })?;
    if let Some(notifier) = notifier {
        super::notify_mind(notifier, &super::mind_view(store, companion)?);
    }
    Ok(result.to_string())
}
