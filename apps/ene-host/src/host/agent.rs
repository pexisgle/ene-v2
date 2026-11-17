//! Agents: the head companions and the work agents, their models, prompts,
//! stream notifications and turn lifecycle.
//!
//! The kernel owns turns and conversations; this module binds an agent to a
//! companion (or a work run), keeps its model connection current and projects
//! the conversation into the host's notification stream.
use super::{
    db::{Result, Store},
    error::HostError,
    mind::{Companion, Role},
    model::{Credentials, ModelFactory},
    notify::Notifier,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::Message as CompletionMessage;
use rig_core::conversation::{Author, AuthorKind, EntryRef};
use rig_core::message::UserContent;
use rig_ecs::model::{Connection as ModelConnection, Models};
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// One agent per companion, addressed by the companion id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
#[require(LastProjectedEntry)]
pub(crate) struct CompanionAgent {
    pub(crate) companion_id: String,
}

/// One agent per open run, addressed by its run id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
#[require(LastProjectedEntry)]
pub(crate) struct WorkAgent {
    pub(crate) work_id: String,
    pub(crate) run_id: String,
    pub(crate) companion_id: String,
}

#[derive(Resource, Default)]
struct Connections {
    revision: u64,
    credential_generation: u64,
    factory: Option<ModelFactory>,
    models: HashMap<(String, String), ModelConnection>,
}

#[derive(Component, Default)]
pub(crate) struct LastProjectedEntry(Option<u64>);

/// The last summary a work run reported through `report`.
#[derive(Resource, Default)]
pub(crate) struct WorkReports {
    pub(crate) completed: HashMap<String, String>,
    /// A failure the hand reported explicitly; the settle then records Failed.
    pub(crate) failed: HashMap<String, String>,
}

/// The id rig-ecs gives the agent, and the name of its session log. It has to
/// be a name Windows accepts: a colon there is an alternate data stream, so
/// `companion:x.jsonl` silently writes to a stream of a file named
/// `companion` and the log reads back empty.
pub(crate) fn companion_agent_id(companion: &str) -> String {
    format!("companion-{companion}")
}

pub(crate) fn work_agent_id(run: &str) -> String {
    format!("work-{run}")
}

pub(crate) fn agent_of(world: &mut World, companion: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &CompanionAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.companion_id == companion)
        .map(|(entity, _)| entity)
}

pub(crate) fn work_agent_of(world: &mut World, run: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &WorkAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.run_id == run)
        .map(|(entity, _)| entity)
}

/// The entity the kernel restored under `id`, which carries the saved
/// conversation but none of the host's role components.
fn restored_agent(world: &mut World, id: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &AgentId)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.0 == id)
        .map(|(entity, _)| entity)
}

/// The kernel's saved component that says which entries were published, set
/// to the restored conversation's last entry so a restart does not republish
/// the transcript as new messages.
fn published_through(world: &World, entity: Entity) -> LastProjectedEntry {
    LastProjectedEntry(world.get::<Conversation>(entity).and_then(|conversation| {
        conversation
            .entries()
            .next_back()
            .map(|entry| entry.id.get())
    }))
}

/// The companion an agent belongs to, whichever role it serves.
fn companion_of<'a>(
    companion: Option<&'a CompanionAgent>,
    work: Option<&'a WorkAgent>,
) -> Option<&'a str> {
    companion
        .map(|agent| agent.companion_id.as_str())
        .or_else(|| work.map(|agent| agent.companion_id.as_str()))
}

/// The system prompt of a companion: static persona only. Everything that
/// changes per turn is injected into the request view instead.
pub(crate) fn system_prompt(companion: &Companion) -> String {
    let mut prompt = format!(
        "あなたはデスクトップ常駐AIコンパニオン「{}」です。ユーザーとは一人のキャラクターとして対話します。\n\
         次の核となる人格・世界観を守ってください:\n{}\n",
        companion.name, companion.mind
    );
    if let Some(persona) = &companion.persona {
        prompt.push_str(&format!("\n話し方・振る舞いの設定:\n{persona}\n"));
    }
    for (label, entries) in [
        ("性格の側面", &companion.profile.facets),
        ("会話例", &companion.profile.examples),
        ("背景", &companion.profile.background),
        ("表現語彙", &companion.profile.expression),
    ] {
        if entries.is_empty() {
            continue;
        }
        prompt.push_str(&format!("\n{label}:\n"));
        for entry in entries {
            prompt.push_str(&format!("- {entry}\n"));
        }
    }
    prompt.push_str(
        "\n振る舞いの原則:\n\
         - 作業を任せた場合も、作業エージェントはあなた自身の「手」です。報告は必ず一人称（「私が今〜しているよ」）で行い、第三者表現を使いません。\n\
         - 沈黙が適切なときは noop を呼びます。\n\
         - 端末操作は device_request とクライアントが宣言した能力ツールで行い、対象端末は指定しません。ホストが束縛します。\n\
         - 記憶は mind_recall / mind_read / mind_update で扱い、推測を事実として保存しません。\n\
         - 現在日時・心の状態・想起記憶・知覚は毎ターン動的コンテキストとして注入されます。\n",
    );
    prompt
}

pub(crate) fn work_prompt(companion: &Companion, instruction: &str) -> String {
    format!(
        "あなたは「{}」の「手」として実務タスクを実行します。ユーザーから見れば「{}」本人です。\n\
         進捗・質問・完了は report ツールで報告してください（session_id と run_id はホストが束縛します）。\n\
         端末操作は device_request を使い、対象端末は指定しません。\n\
         現在の指示:\n{instruction}\n",
        companion.name, companion.name
    )
}

/// Spawns the agent of a companion if it does not exist yet, or adopts the
/// one the kernel restored under the companion's stable id.
pub(crate) fn spawn_companion(world: &mut World, companion: &Companion) -> Entity {
    if let Some(entity) = agent_of(world, &companion.id) {
        return entity;
    }
    let id = companion_agent_id(&companion.id);
    let entity = match restored_agent(world, &id) {
        Some(entity) => entity,
        None => world
            .spawn((
                Name::new(format!("companion:{}", companion.id)),
                Agent,
                AgentId(id),
            ))
            .id(),
    };
    let published = published_through(world, entity);
    world.entity_mut(entity).insert((
        CompanionAgent {
            companion_id: companion.id.clone(),
        },
        SystemPrompt(system_prompt(companion)),
        crate::host::client::parent_tool_access(),
    ));
    world.entity_mut(entity).insert(published);
    entity
}

pub(crate) fn spawn_work_agent(
    world: &mut World,
    companion: &Companion,
    work_id: &str,
    run_id: &str,
    instruction: &str,
) -> Entity {
    if let Some(entity) = work_agent_of(world, run_id) {
        return entity;
    }
    let id = work_agent_id(run_id);
    let entity = match restored_agent(world, &id) {
        Some(entity) => entity,
        None => world
            .spawn((Name::new(format!("work:{run_id}")), Agent, AgentId(id)))
            .id(),
    };
    let published = published_through(world, entity);
    world.entity_mut(entity).insert((
        WorkAgent {
            work_id: work_id.to_owned(),
            run_id: run_id.to_owned(),
            companion_id: companion.id.clone(),
        },
        SystemPrompt(work_prompt(companion, instruction)),
        ToolAccess::Only(vec!["report".into(), "device_request".into()]),
    ));
    world.entity_mut(entity).insert(published);
    entity
}

/// Binds the agent's model when its assignment or the settings changed.
pub(crate) fn ensure_model(world: &mut World, entity: Entity) -> Result<()> {
    world.init_resource::<Connections>();
    let (companion_id, role) = match world.get::<CompanionAgent>(entity) {
        Some(agent) => (agent.companion_id.clone(), Role::Companion),
        None => match world.get::<WorkAgent>(entity) {
            Some(agent) => (agent.companion_id.clone(), Role::Task),
            None => return Err(HostError::Internal(("an agent without a role").into())),
        },
    };
    let store = world.resource::<Store>().clone();
    let companion = store
        .companion(&companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let assignment = companion.model(role).map(|assignment| {
        (
            assignment.connection_id.clone(),
            assignment.model_id.clone(),
        )
    });
    let revision = store.settings_revision()?;
    let credentials = world.resource::<Credentials>().clone();
    let credential_generation = credentials.generation();
    let factory = world.resource::<ModelFactory>().clone();
    {
        let mut cache = world.resource_mut::<Connections>();
        if cache.revision != revision
            || cache.credential_generation != credential_generation
            || cache
                .factory
                .as_ref()
                .is_none_or(|previous| !std::sync::Arc::ptr_eq(&previous.0, &factory.0))
        {
            cache.models.clear();
            cache.revision = revision;
            cache.credential_generation = credential_generation;
            cache.factory = Some(factory.clone());
        }
    }
    let cached = assignment
        .as_ref()
        .and_then(|key| world.resource::<Connections>().models.get(key).cloned());
    let connection = match cached {
        Some(connection) => connection,
        None => {
            let catalog = world.resource::<Models>().0.catalog().clone();
            let providers = world
                .get_resource::<crate::host::providers::Providers>()
                .cloned()
                .unwrap_or_default();
            let model = (factory.0)(&store, &credentials, &catalog, &companion, role, &providers)?;
            let spec = std::sync::Arc::new(model.spec().cloned().ok_or_else(|| {
                HostError::Internal(("a completion model without catalog facts").into())
            })?);
            let handler =
                rig_core::serve::ErasedHandler::new(rig_core::serve::adapters::ModelAdapter::<
                    rig_core::operation::Completion,
                >::new(spec.reference(), model));
            let connection = ModelConnection {
                spec,
                handler: rig_ecs::effects::Handler(handler),
            };
            if let Some(key) = assignment {
                world
                    .resource_mut::<Connections>()
                    .models
                    .insert(key, connection.clone());
            }
            connection
        }
    };
    world.entity_mut(entity).insert(connection);
    Ok(())
}

/// Tells the clients that a saved companion has no usable model yet.
pub(crate) fn notice_model_unavailable(world: &mut World, companion_id: &str, error: &HostError) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(
        Some(companion_id),
        "model_unavailable",
        &crate::host::db::truncate(&error.to_string(), 300),
    );
}

/// Hands a turn to one agent, but only when its model is bound. Voice and the
/// schedule loop deliver turns without going through `chat`, which is where a
/// typed message asks for a model; without this a turn can run on the model the
/// agent was bound to before its assignment or settings changed.
pub(crate) fn deliver(
    world: &mut World,
    entity: Entity,
    companion_id: &str,
    text: String,
    mode: DeliveryMode,
    author: Author,
) -> bool {
    if let Err(error) = ensure_model(world, entity) {
        notice_model_unavailable(world, companion_id, &error);
        return false;
    }
    world.trigger(Deliver::new(entity, text, mode).with_author(author));
    true
}

/// Rebuilds the system prompt after a companion edit, and drops the agent when
/// the companion is gone.
pub(crate) fn sync_companion(world: &mut World, companion_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let entity = agent_of(world, companion_id);
    match store.companion(companion_id)? {
        Some(companion) => {
            let entity = entity.unwrap_or_else(|| spawn_companion(world, &companion));
            world
                .entity_mut(entity)
                .insert(SystemPrompt(system_prompt(&companion)));
            world.entity_mut(entity).remove::<ModelConnection>();
            // The store is the source of truth and the binding is derived, so a
            // model the host cannot bind yet is a notice: a turn, an edit or the
            // next start retries it. A storage failure still fails.
            match ensure_model(world, entity) {
                Ok(()) => Ok(()),
                Err(error) if error.is_storage() => Err(error),
                Err(error) => {
                    notice_model_unavailable(world, companion_id, &error);
                    Ok(())
                }
            }
        }
        None => {
            if let Some(entity) = entity {
                world.trigger(Retire { entity });
            }
            Ok(())
        }
    }
}

/// Reconciliation after the first frame: one agent per companion, one per open
/// run, and no agent whose companion or run is gone. A store failure ends the
/// start, because read as an empty list it would retire every restored agent.
pub(crate) fn reconcile(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let companions = store.companions()?;
    let existing: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<CompanionAgent>, With<WorkAgent>)>>()
        .iter(world)
        .collect();
    for companion in &companions {
        spawn_companion(world, companion);
        // Work agents for runs that are still open.
        let Ok(sessions) = store.work_sessions(&companion.id) else {
            continue;
        };
        for session in sessions {
            let Ok(Some(run)) = store.active_run(&session.id) else {
                continue;
            };
            spawn_work_agent(world, companion, &session.id, &run.run_id, &run.instruction);
        }
    }
    // Retire the kernel's default agent and any agent without a role.
    let stray: Vec<Entity> = world
        .query_filtered::<Entity, (With<Agent>, Without<CompanionAgent>, Without<WorkAgent>)>()
        .iter(world)
        .collect();
    for entity in stray {
        world.trigger(Retire { entity });
    }
    // Retire agents whose run closed or whose companion disappeared.
    for entity in existing {
        let keep = world.get::<CompanionAgent>(entity).is_some_and(|agent| {
            store
                .companion(&agent.companion_id)
                .ok()
                .flatten()
                .is_some()
        }) || world.get::<WorkAgent>(entity).is_some_and(|agent| {
            store
                .run(&agent.run_id)
                .ok()
                .flatten()
                .is_some_and(|run| crate::host::db::work::open_state(run.state))
        });
        if !keep {
            world.trigger(Retire { entity });
        }
    }
    // Bind every remaining agent's model; a missing model is a notice, not a
    // startup failure.
    let agents: Vec<(Entity, String)> = {
        let mut query = world.query::<(Entity, Option<&CompanionAgent>, Option<&WorkAgent>)>();
        query
            .iter(world)
            .filter_map(|(entity, companion, work)| {
                companion_of(companion, work).map(|companion| (entity, companion.to_owned()))
            })
            .collect()
    };
    for (entity, companion) in agents {
        if let Err(error) = ensure_model(world, entity) {
            notice_model_unavailable(world, &companion, &error);
        }
    }
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(None, "host_ready", "the host restored its state");
    Ok(())
}

/// The conversation text of one entry, when it belongs in the transcript.
pub(crate) fn entry_text(entry: &EntryRef<'_>) -> Option<String> {
    match entry.message {
        CompletionMessage::User { content } => {
            let mut text = String::new();
            for item in content {
                if let UserContent::Text(item) = item {
                    text.push_str(&item.text);
                }
            }
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        CompletionMessage::Assistant(reply) => {
            let text = reply.text();
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        CompletionMessage::System { .. } => None,
    }
}

pub(crate) fn role_of(entry: &EntryRef<'_>) -> w::MessageRole {
    match (entry.author.map(|author| &author.kind), entry.message) {
        (None, CompletionMessage::User { .. }) => w::MessageRole::User,
        (None, CompletionMessage::System { .. }) => w::MessageRole::Context,
        (None, CompletionMessage::Assistant(_)) => w::MessageRole::Assistant,
        (Some(AuthorKind::User), _) => w::MessageRole::User,
        (Some(AuthorKind::Agent), _) => w::MessageRole::Agent,
        (Some(AuthorKind::Plugin(_)), _) | (Some(AuthorKind::Rig), _) => w::MessageRole::Context,
    }
}

/// The agent rows a message projection needs.
type AgentRows = (
    &'static AgentId,
    &'static Conversation,
    Option<&'static CompanionAgent>,
    Option<&'static WorkAgent>,
    &'static mut LastProjectedEntry,
);

/// The agent rows a turn projection needs.
type TurnAgentRows = (
    &'static AgentId,
    Option<&'static CompanionAgent>,
    Option<&'static WorkAgent>,
);

type UnpublishedMessages = Or<(Changed<Conversation>, Added<LastProjectedEntry>)>;

/// Projects every new conversation entry into `MessageCreated` notifications.
fn publish_messages(
    mut agents: Query<AgentRows, UnpublishedMessages>,
    mut notifier: ResMut<Notifier>,
) {
    for (_, conversation, companion, work, mut published) in &mut agents {
        let companion_id = companion_of(companion, work).unwrap_or_default();
        let work_id = work.map(|agent| agent.work_id.as_str()).unwrap_or_default();
        let start = conversation
            .entries()
            .rposition(|entry| published.0.is_some_and(|last| entry.id.get() <= last))
            .map_or(0, |at| at + 1);
        for entry in conversation.entries().skip(start) {
            published.0 = Some(entry.id.get());
            let Some(text) = entry_text(&entry) else {
                continue;
            };
            let message = super::chat::message(companion_id, work_id, &entry, text);
            notifier.push(
                Some(companion_id),
                None,
                w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                    message: Some(message).into(),
                    ..Default::default()
                })),
            );
        }
    }
}

/// Reports a turn that appeared this frame as streaming.
fn turn_started(
    turns: Query<(&Turn, &TurnOf, Option<&ForAgent>), Added<TurnOf>>,
    agents: Query<(&AgentId, Option<&CompanionAgent>)>,
    mut notifier: ResMut<Notifier>,
) {
    for (turn, of, _) in &turns {
        let companion = agents
            .get(of.0)
            .ok()
            .and_then(|(_, agent)| agent.map(|agent| agent.companion_id.clone()))
            .unwrap_or_default();
        notifier.push(
            Some(companion.as_str()),
            None,
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: companion.clone(),
                turn_id: turn.start.get().to_string(),
                status: w::TurnStatus::Streaming.into(),
                failure: None.into(),
                ..Default::default()
            })),
        );
    }
}

/// Publishes the final turn event once a turn settles.
fn turn_settled(
    mut commands: Commands,
    settled: Query<(Entity, &TurnOutcome, &ForAgent), Added<Settled>>,
    turns: Query<&Turn>,
    agents: Query<TurnAgentRows>,
    mut notifier: ResMut<Notifier>,
) {
    for (turn_entity, outcome, of) in &settled {
        let Ok((id, companion, work)) = agents.get(of.0) else {
            continue;
        };
        let companion_id = companion_of(companion, work).unwrap_or_default();
        let turn_id = turns
            .get(turn_entity)
            .map(|turn| turn.start.get().to_string())
            .unwrap_or_else(|_| id.0.clone());
        let (status, failure) = match outcome {
            TurnOutcome::Answered(_) => (w::TurnStatus::Answered, None),
            TurnOutcome::Stopped(_) => (w::TurnStatus::Stopped, None),
            TurnOutcome::Failed(why) => (
                w::TurnStatus::Failed,
                Some(w::Failure {
                    code: "turn_failed".into(),
                    message: crate::host::db::truncate(why, 500),
                    ..Default::default()
                }),
            ),
        };
        notifier.push(
            Some(companion_id),
            None,
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: companion_id.to_owned(),
                turn_id,
                status: status.into(),
                failure: failure.into(),
                ..Default::default()
            })),
        );
        if let Some(companion) = companion {
            let companion_id = companion.companion_id.clone();
            // A stopped or failed turn must not speak its flushed remainder.
            let answered = matches!(outcome, TurnOutcome::Answered(_));
            commands.queue(move |world: &mut World| {
                if answered {
                    super::voice::on_turn_end(world, &companion_id);
                } else {
                    let _ = super::voice::flush(world, &companion_id, w::FlushReason::Cancelled);
                }
            });
        }
        if let Some(work) = work {
            let run_id = work.run_id.clone();
            let outcome = outcome.clone();
            commands.queue(move |world: &mut World| {
                crate::host::work::settle_run(world, run_id, outcome);
            });
        }
    }
}

/// Reports text deltas of ordinary conversation turns only: never reasoning,
/// summaries or child-agent output.
fn stream_deltas(
    mut deltas: MessageReader<StreamDelta>,
    calls: Query<(), (With<TurnCall>, Without<Settled>)>,
    agents: Query<(&AgentId, Option<&CompanionAgent>)>,
    mut notifier: ResMut<Notifier>,
    mut commands: Commands,
) {
    for delta in deltas.read() {
        if !calls.contains(delta.effect) {
            continue;
        }
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        // Only a companion speaks to the user; a work agent's text is a report
        // to the head, never a transcript delta of its own.
        let Some(companion) = delta
            .agent
            .and_then(|agent| agents.get(agent).ok())
            .and_then(|(_, companion)| companion.map(|agent| agent.companion_id.clone()))
        else {
            continue;
        };
        notifier.push(
            Some(companion.as_str()),
            None,
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                companion_id: companion.clone(),
                stream_id: delta.effect.to_string(),
                delta: text.clone(),
                ..Default::default()
            })),
        );
        let voice_companion = companion.clone();
        let voice_text = text.clone();
        commands.queue(move |world: &mut World| {
            super::voice::on_stream_text(world, &voice_companion, &voice_text);
        });
    }
}

/// Answers device tool calls whose client never responded.
fn expire_devices(world: &mut World) {
    let now = std::time::Instant::now();
    let expired: Vec<String> = {
        let waiting = world.resource::<super::resources::Waiting>();
        waiting
            .by_operation
            .iter()
            .filter(|(_, waiter)| waiter.deadline <= now)
            .map(|(operation, _)| operation.clone())
            .collect()
    };
    for operation in expired {
        super::client::device::expire(world, &operation);
    }
}

/// The host's agent plugin: projection and device timeouts.
pub(crate) struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Connections>()
            .init_resource::<ModelFactory>()
            .init_resource::<WorkReports>()
            .add_systems(
                Update,
                (
                    publish_messages.after(AgentLoop),
                    turn_started.after(AgentLoop),
                    turn_settled.after(AgentLoop),
                    expire_devices.after(AgentLoop),
                ),
            )
            .add_systems(AgentPass, stream_deltas.in_set(ReviewSystems::Edit));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::conversation::{Conversation as History, Entry, RequestId};

    #[test]
    fn transcript_text_keeps_full_text_blocks_without_separators_or_reasoning() {
        use rig_core::completion::{AssistantContent, AssistantMessage};

        let long = "full\ntext ".repeat(30);
        let history = History::from_messages([
            CompletionMessage::User {
                content: vec![UserContent::text("  user"), UserContent::text("\ntext  ")],
            },
            CompletionMessage::Assistant(AssistantMessage::new(vec![
                AssistantContent::text("  first"),
                AssistantContent::reasoning("private reasoning"),
                AssistantContent::text("second\n"),
                AssistantContent::text(long.clone()),
            ])),
            CompletionMessage::Assistant(AssistantMessage::new(vec![
                AssistantContent::reasoning("private reasoning"),
                AssistantContent::text(" \n\t"),
            ])),
            CompletionMessage::system("hidden system context"),
        ]);
        let text: Vec<_> = history.entries().map(|entry| entry_text(&entry)).collect();
        assert_eq!(text[0].as_deref(), Some("user\ntext"));
        assert_eq!(
            text[1],
            Some(format!("firstsecond\n{long}").trim().to_owned())
        );
        assert_eq!(&text[2..], &[None, None]);
    }

    #[tokio::test]
    async fn connected_assignments_observe_cloned_credential_mutations() {
        use crate::host::{model, tests::Harness};
        use rig_core::{
            completion::CompletionRequest,
            effect::{Answered, EffectKind},
            serve::serve_inline,
            test_utils::{MockCompletionModel, MockTurn},
        };
        use std::sync::Arc;

        let mut harness = Harness::new([]);
        let store = harness.store();
        let mut companion = store.companion(&harness.companion).unwrap().unwrap();
        companion.assignments.companion = crate::host::mind::Assignment {
            connection_id: "credential-model".into(),
            model_id: "fake".into(),
        };
        store
            .update_companion(&companion, companion.revision)
            .unwrap();
        let credentials = Credentials::new(true);
        let writer = credentials.clone();
        harness.app.world_mut().insert_resource(credentials);
        let spec = rig_core::catalog::Catalog::builtin()
            .resolve("openai/gpt-4o-mini")
            .unwrap()
            .shared();
        harness
            .app
            .world_mut()
            .insert_resource(ModelFactory(Arc::new(move |_, credentials, _, _, _, _| {
                let text = credentials
                    .secret("credential-model")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "absent".into());
                Ok(
                    MockCompletionModel::from_turns((0..8).map(|_| MockTurn::text(text.clone())))
                        .priced_as(spec.clone())
                        .into(),
                )
            })));
        let entity = harness.companion_agent();
        for expected in ["old-key", "new-key", "absent", "direct-key", "absent"] {
            match expected {
                "old-key" | "new-key" => {
                    model::save_secret(&writer, "credential-model", expected).unwrap()
                }
                "direct-key" => writer.remember("credential-model", expected.into()),
                _ => model::delete_secret(&writer, "credential-model").unwrap(),
            }
            ensure_model(harness.app.world_mut(), entity).unwrap();
            let handler = harness
                .app
                .world()
                .get::<ModelConnection>(entity)
                .unwrap()
                .handler
                .0
                .clone();
            let answer = serve_inline(
                &handler,
                EffectKind::Completion {
                    request: CompletionRequest::new("hi").into(),
                    stream: false,
                },
            )
            .await
            .unwrap();
            let Answered::Completion(answer) = answer else {
                panic!("expected completion")
            };
            assert_eq!(answer.text(), expected);
        }
    }

    #[test]
    fn messages_publish_once_oldest_first_across_additions_and_restores() {
        let mut app = App::new();
        app.insert_resource(Notifier::new())
            .add_systems(Update, publish_messages);
        let mut events = app.world().resource::<Notifier>().subscribe();
        let mut history = History::new();
        history.push(Entry::new(CompletionMessage::user("hello")).by(Author {
            request: Some(RequestId("request-1".into())),
            ..Author::user()
        }));
        history.push(CompletionMessage::system("hidden context"));
        history.push(CompletionMessage::assistant("answer"));
        let entity = app
            .world_mut()
            .spawn((
                AgentId("work-run".into()),
                Conversation(history),
                WorkAgent {
                    work_id: "work-1".into(),
                    run_id: "run-1".into(),
                    companion_id: "ene".into(),
                },
            ))
            .id();
        app.update();
        app.world_mut().resource_mut::<Notifier>().flush();
        let messages: Vec<w::Message> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event.event.unwrap() {
                w::notification::Event::MessageCreated(created) => {
                    created.message.as_option().unwrap().clone()
                }
                _ => panic!("only transcript messages are published"),
            })
            .collect();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", "answer"]
        );
        assert!(messages[0].sequence < messages[1].sequence);
        assert_eq!(messages[0].request_id.as_deref(), Some("request-1"));
        assert_eq!(messages[0].role, w::MessageRole::User);
        assert_eq!(messages[0].companion_id, "ene");
        assert_eq!(messages[0].work_id, "work-1");
        assert_eq!(messages[0].author, Author::user().label());
        assert!(!messages[0].created_at.is_empty());

        let mut restored = app.world().get::<Conversation>(entity).unwrap().0.view();
        restored.push(CompletionMessage::system("more hidden context"));
        app.world_mut()
            .entity_mut(entity)
            .insert(Conversation(restored.view()));
        app.update();
        app.world_mut().resource_mut::<Notifier>().flush();
        assert!(events.try_recv().is_err());

        restored.extend([
            CompletionMessage::user("next"),
            CompletionMessage::assistant("new answer"),
        ]);
        app.world_mut()
            .entity_mut(entity)
            .insert(Conversation(restored));
        app.update();
        app.update();
        app.world_mut().resource_mut::<Notifier>().flush();
        let texts: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event.event.unwrap() {
                w::notification::Event::MessageCreated(created) => {
                    created.message.as_option().unwrap().text.clone()
                }
                _ => panic!("only transcript messages are published"),
            })
            .collect();
        assert_eq!(texts, ["next", "new answer"]);
    }
}
