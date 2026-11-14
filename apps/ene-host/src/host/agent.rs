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
    model::{self, Credentials},
    notify::Notifier,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::{AssistantContent, Message as CompletionMessage};
use rig_core::conversation::{AuthorKind, EntryRef};
use rig_core::message::UserContent;
use rig_ecs::model::{Connection as ModelConnection, Models};
use rig_ecs::prelude::*;
use std::collections::HashMap;

/// One agent per companion, addressed by the companion id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompanionAgent {
    pub(crate) companion_id: String,
}

/// One agent per open run, addressed by its run id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkAgent {
    pub(crate) work_id: String,
    pub(crate) run_id: String,
    pub(crate) companion_id: String,
}

/// What the agent's model connection was built from, to keep it current.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) connection_id: String,
    pub(crate) model_id: String,
    pub(crate) settings_revision: u64,
}

/// The last conversation entry this host projected per agent.
#[derive(Resource, Default)]
pub(crate) struct Published {
    cursor: HashMap<String, u64>,
}

/// The last summary a work run reported through `report`.
#[derive(Resource, Default)]
pub(crate) struct WorkReports {
    pub(crate) completed: HashMap<String, String>,
    /// A failure the hand reported explicitly; the settle then records Failed.
    pub(crate) failed: HashMap<String, String>,
}

pub(crate) fn companion_agent_id(companion: &str) -> String {
    format!("companion:{companion}")
}

pub(crate) fn work_agent_id(run: &str) -> String {
    format!("work:{run}")
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

/// Spawns the agent of a companion if it does not exist yet.
pub(crate) fn spawn_companion(world: &mut World, companion: &Companion) -> Entity {
    if let Some(entity) = agent_of(world, &companion.id) {
        return entity;
    }
    world
        .spawn((
            Name::new(format!("companion:{}", companion.id)),
            Agent,
            AgentId(companion_agent_id(&companion.id)),
            CompanionAgent {
                companion_id: companion.id.clone(),
            },
            SystemPrompt(system_prompt(companion)),
            ToolAccess::All,
        ))
        .id()
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
    world
        .spawn((
            Name::new(format!("work:{run_id}")),
            Agent,
            AgentId(work_agent_id(run_id)),
            WorkAgent {
                work_id: work_id.to_owned(),
                run_id: run_id.to_owned(),
                companion_id: companion.id.clone(),
            },
            SystemPrompt(work_prompt(companion, instruction)),
            ToolAccess::Only(vec!["report".into(), "device_request".into()]),
        ))
        .id()
}

/// Binds the agent's model when its assignment or the settings changed.
pub(crate) fn ensure_model(world: &mut World, entity: Entity) -> Result<()> {
    #[cfg(test)]
    if let Some(test) = world.get_resource::<TestModel>() {
        let bound = (test.0)();
        world.entity_mut(entity).insert((
            bound.connection,
            Binding {
                connection_id: "test".into(),
                model_id: "test".into(),
                settings_revision: 0,
            },
        ));
        return Ok(());
    }
    let (companion_id, role) = match world.get::<CompanionAgent>(entity) {
        Some(agent) => (agent.companion_id.clone(), Role::Companion),
        None => match world.get::<WorkAgent>(entity) {
            Some(agent) => (agent.companion_id.clone(), Role::Task),
            None => return Err(HostError::internal("an agent without a role")),
        },
    };
    let store = world.resource::<Store>().clone();
    let companion = store
        .companion(&companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let assignment = companion
        .model(role)
        .ok_or_else(|| HostError::unavailable("no model is assigned to this role"))?;
    let settings_revision = store.settings_revision()?;
    let current = world.get::<Binding>(entity).cloned();
    let current_ok = current.as_ref().is_some_and(|binding| {
        binding.connection_id == assignment.connection_id
            && binding.model_id == assignment.model_id
            && binding.settings_revision == settings_revision
    }) && world.get::<ModelConnection>(entity).is_some();
    if current_ok {
        return Ok(());
    }
    let credentials = world.resource::<Credentials>().clone();
    let catalog = world.resource::<Models>().0.catalog().clone();
    let bound = model::connect(&store, &credentials, &catalog, &companion, role)?;
    world.entity_mut(entity).insert((
        bound.connection,
        Binding {
            connection_id: assignment.connection_id.clone(),
            model_id: assignment.model_id.clone(),
            settings_revision,
        },
    ));
    Ok(())
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
            world.entity_mut(entity).remove::<Binding>();
            ensure_model(world, entity)
        }
        None => {
            if let Some(entity) = entity {
                world.trigger(Retire { entity });
            }
            Ok(())
        }
    }
}

/// Startup reconciliation: one agent per companion, one per open run, and no
/// agent whose companion or run is gone.
pub(crate) fn reconcile(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let companions = store.companions().unwrap_or_default();
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
        .query_filtered::<Entity, (Without<CompanionAgent>, Without<WorkAgent>)>()
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
            let mut notifier = world.resource_mut::<Notifier>();
            notifier.notice(
                Some(&companion),
                "model_unavailable",
                &crate::host::db::truncate(&error.to_string(), 300),
            );
        }
    }
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(None, "host_ready", "the host restored its state");
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
            let mut text = String::new();
            for item in &reply.content {
                if let AssistantContent::Text(item) = item {
                    text.push_str(&item.text);
                }
            }
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        CompletionMessage::System { .. } => None,
    }
}

fn role_of(entry: &EntryRef<'_>) -> w::MessageRole {
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
);

/// The agent rows a turn projection needs.
type TurnAgentRows = (
    &'static AgentId,
    Option<&'static CompanionAgent>,
    Option<&'static WorkAgent>,
);

/// Projects every new conversation entry into `MessageCreated` notifications.
fn publish_messages(
    agents: Query<AgentRows>,
    mut notifier: ResMut<Notifier>,
    mut published: ResMut<Published>,
) {
    for (id, conversation, companion, work) in &agents {
        let companion_id = companion_of(companion, work).unwrap_or_default();
        let last = published.cursor.get(&id.0).copied().unwrap_or(0);
        let mut newest = last;
        for entry in conversation.entries() {
            let sequence = entry.id.get();
            newest = newest.max(sequence);
            if sequence <= last {
                continue;
            }
            let Some(text) = entry_text(&entry) else {
                continue;
            };
            let message = w::Message {
                id: sequence.to_string(),
                companion_id: companion_id.to_owned(),
                work_id: work.map(|agent| agent.work_id.clone()).unwrap_or_default(),
                role: role_of(&entry).into(),
                text,
                created_at: entry.made.to_string(),
                sequence,
                request_id: entry
                    .author
                    .and_then(|author| author.request.as_ref())
                    .map(|request| request.0.clone()),
                author: entry
                    .author
                    .map(|author| author.label())
                    .unwrap_or_default(),
                ..Default::default()
            };
            notifier.push(
                Some(companion_id),
                None,
                w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                    message: Some(message).into(),
                    ..Default::default()
                })),
            );
        }
        if newest > last {
            published.cursor.insert(id.0.clone(), newest);
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

/// The host's agent plugin: reconciliation, projection and device timeouts.
pub(crate) struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Published>()
            .init_resource::<WorkReports>()
            .add_systems(Startup, reconcile)
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
    use crate::host::json::JsonDoc;
    use crate::host::mind::{Profile, State};

    fn companion() -> Companion {
        Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Default::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn prompts_carry_the_persona_and_the_one_persona_rule() {
        let prompt = system_prompt(&companion());
        assert!(prompt.contains("kind"));
        assert!(prompt.contains("一人称"));
        assert!(prompt.contains("noop"));
        let work = work_prompt(&companion(), "list the folder");
        assert!(work.contains("list the folder"));
        assert!(work.contains("report"));
    }

    #[test]
    fn message_roles_follow_the_author() {
        let conversation =
            rig_core::conversation::Conversation::from_messages([CompletionMessage::user("hello")]);
        let entry = conversation.entries().next().expect("one entry");
        assert_eq!(role_of(&entry), w::MessageRole::User);
        assert_eq!(entry_text(&entry).as_deref(), Some("hello"));

        let conversation =
            rig_core::conversation::Conversation::from_messages([CompletionMessage::assistant(
                "hi",
            )]);
        let entry = conversation.entries().next().expect("one entry");
        assert_eq!(role_of(&entry), w::MessageRole::Assistant);
        assert_eq!(entry_text(&entry).as_deref(), Some("hi"));
    }
}

/// Test-only model injection: the host never builds provider connections in
/// a unit test.
#[cfg(test)]
#[derive(Resource, Clone)]
pub(crate) struct TestModel(
    pub(crate) std::sync::Arc<dyn Fn() -> super::model::Bound + Send + Sync>,
);
