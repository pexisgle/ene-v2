//! Agent identity, prompts and the Rig kernel extension points.
use super::{
    body, chat, client, compaction,
    db::{Result, Store},
    mind,
    model::{self, Binding, Refusal},
    notify::Notifier,
    permission,
    resources::{CompactionQueue, ConsolidationQueue},
    wire, work,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::Author;
use rig_ecs::{WriteLogs, prelude::*};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Which conversation an agent is, independent of Rig's opaque id.
///
/// Saved with the session, so a restart never parses an id prefix.
#[derive(Component, Clone, Debug, PartialEq, Eq, Hash, Reflect, Serialize, Deserialize)]
#[reflect(Component, Saved, Clone, Serialize, Deserialize)]
pub(crate) enum AgentKey {
    Parent { companion: String },
    Work { companion: String, work: String },
}

impl AgentKey {
    pub(crate) fn parent(companion: impl Into<String>) -> Self {
        Self::Parent {
            companion: companion.into(),
        }
    }

    pub(crate) fn work(companion: impl Into<String>, work: impl Into<String>) -> Self {
        Self::Work {
            companion: companion.into(),
            work: work.into(),
        }
    }

    pub(crate) fn companion(&self) -> &str {
        match self {
            Self::Parent { companion } | Self::Work { companion, .. } => companion,
        }
    }

    pub(crate) fn work_id(&self) -> Option<&str> {
        match self {
            Self::Parent { .. } => None,
            Self::Work { work, .. } => Some(work),
        }
    }

    pub(crate) fn is_work(&self) -> bool {
        matches!(self, Self::Work { .. })
    }

    pub(crate) fn agent_id(&self) -> String {
        match self {
            Self::Parent { companion } => format!("parent-{companion}"),
            Self::Work { work, .. } => format!("work-{work}"),
        }
    }
}

pub(crate) struct KernelPlugin;

impl Plugin for KernelPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<AgentKey>()
            .add_observer(turn_input)
            .add_observer(turn_started)
            .add_observer(turn_settled)
            .add_observer(follow_successor::<Binding>)
            .add_observer(follow_successor::<mind::Batch>)
            .add_observer(follow_successor::<compaction::SummaryJob>)
            .add_observer(permission::hold_released)
            .add_systems(
                Update,
                (client::sweep, mind::start_consolidation, compaction::start).before(AgentLoop),
            )
            .add_systems(AgentPass, bindings.in_set(PrepareSystems::Compose))
            .add_systems(AgentPass, prepare.in_set(PrepareSystems::Edit))
            .add_systems(
                AgentPass,
                compaction::inject
                    .in_set(PrepareSystems::Edit)
                    .after(prepare),
            )
            .add_systems(
                AgentPass,
                mind::inject_context
                    .in_set(PrepareSystems::Edit)
                    .after(compaction::inject),
            )
            .add_systems(AgentPass, compaction::fit.in_set(PrepareSystems::Fit))
            .add_systems(AgentPass, permission::gate.in_set(Stage::Gate))
            .add_systems(AgentPass, streams.in_set(Stage::Review))
            .add_systems(
                AgentPass,
                (output_limit, compaction::apply, mind::apply_consolidation)
                    .in_set(ReviewSystems::Edit),
            )
            .add_systems(PostUpdate, recalled)
            .add_systems(Last, changes.after(WriteLogs));
        mind::register_tools(app);
        work::register_tools(app);
        client::register_tools(app);
        body::register_tools(app);
    }
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let agents: Vec<(Entity, Option<AgentKey>)> = world
        .query::<(Entity, Option<&AgentKey>, &Agent)>()
        .iter(world)
        .map(|(entity, key, _)| (entity, key.cloned()))
        .collect();
    let default_companion = store.companions()?.into_iter().next();
    for (entity, key) in agents {
        match key {
            Some(AgentKey::Parent { companion }) => {
                if store.companion(&companion)?.is_some() {
                    chat::attach_agent(world, entity, &AgentKey::parent(companion));
                } else {
                    world.trigger(Retire { entity });
                }
            }
            Some(AgentKey::Work { companion, work }) => {
                let exists = store
                    .work(&work)?
                    .is_some_and(|session| session.companion_id == companion && !session.closed);
                if exists {
                    chat::attach_agent(world, entity, &AgentKey::work(companion, work));
                } else {
                    world.trigger(Retire { entity });
                }
            }
            None => {
                let Some(companion) = &default_companion else {
                    world.trigger(Retire { entity });
                    continue;
                };
                let key = AgentKey::parent(&companion.id);
                world
                    .entity_mut(entity)
                    .insert((key.clone(), AgentId(key.agent_id())));
                chat::attach_agent(world, entity, &key);
            }
        }
    }
    Ok(())
}

fn turn_input(
    started: On<Add<TurnOf>>,
    turns: Query<(&TurnOf, &Turn)>,
    mut conversations: Query<&mut Conversation>,
) {
    let Ok((agent, turn)) = turns.get(started.entity) else {
        return;
    };
    let Ok(mut conversation) = conversations.get_mut(agent.0) else {
        return;
    };
    if let Some(input) = chat::input_of(&conversation, turn.start) {
        conversation.add_aside(Aside::new(
            "ene.turn_input",
            Author::rig(),
            json!({"start": turn.start.get(), "input_id": input}),
        ));
    }
}

fn turn_started(
    started: On<Add<TurnOf>>,
    turns: Query<&TurnOf>,
    keys: Query<&AgentKey>,
    mut commands: Commands,
) {
    let Ok(&TurnOf(agent)) = turns.get(started.entity) else {
        return;
    };
    let Ok(key) = keys.get(agent) else {
        return;
    };
    let key = key.clone();
    let turn_entity = started.entity;
    commands.queue(move |world: &mut World| {
        let Some(turn) = world.get::<Turn>(turn_entity) else {
            return;
        };
        let event = w::TurnEvent {
            companion_id: key.companion().into(),
            turn_id: format!("{}:{}", key.agent_id(), turn.start.get()),
            status: "streaming".into(),
            ..Default::default()
        };
        world.resource_mut::<Notifier>().push(
            Some(key.companion().into()),
            None,
            w::notification::Event::TurnEvent(Box::new(event)),
        );
    });
}

fn turn_settled(
    settled: On<Add<Settled>>,
    turns: Query<&ForAgent, With<Turn>>,
    mut commands: Commands,
) {
    let Ok(&ForAgent(agent)) = turns.get(settled.entity) else {
        return;
    };
    commands.queue(move |world: &mut World| {
        let Some(key) = world.get::<AgentKey>(agent).cloned() else {
            return;
        };
        if !key.is_work() {
            world
                .resource_mut::<ConsolidationQueue>()
                .0
                .insert(key.companion().into());
            world
                .resource_mut::<CompactionQueue>()
                .0
                .insert(key.companion().into());
            if let Err(error) = work::finalize_reports(world, key.companion())
                && error.is_storage()
            {
                super::fail(world, error.to_string());
            }
        } else if let Err(error) = work::report_work(world, agent)
            && error.is_storage()
        {
            super::fail(world, error.to_string());
        }
        let Some(conversation) = world.get::<Conversation>(agent) else {
            return;
        };
        let Some((_, record)) = conversation.turns().next_back() else {
            return;
        };
        let start = record.start;
        let (state, failure) = chat::turn_state(record.end.as_ref());
        if let Some((id, text)) = chat::last_assistant(conversation, start) {
            world.resource_mut::<Notifier>().push(
                Some(key.companion().into()),
                None,
                w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                    message: w::Message {
                        id: id.to_string(),
                        companion_id: key.companion().into(),
                        work_id: key.work_id().unwrap_or_default().into(),
                        role: "assistant".into(),
                        text,
                        sequence: id,
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                })),
            );
        }
        let event = w::TurnEvent {
            companion_id: key.companion().into(),
            turn_id: format!("{}:{}", key.agent_id(), start.get()),
            status: state.as_str().into(),
            failure: failure.as_ref().map(wire::failure).into(),
            ..Default::default()
        };
        world.resource_mut::<Notifier>().push(
            Some(key.companion().into()),
            None,
            w::notification::Event::TurnEvent(Box::new(event)),
        );
    });
}

fn bindings(world: &mut World) {
    let calls: Vec<(Entity, Option<Entity>)> = world
        .query_filtered::<(Entity, Option<&ForAgent>), ReadyToStart>()
        .iter(world)
        .map(|(call, agent)| (call, agent.map(|agent| agent.0)))
        .collect();
    let mut ready = Vec::new();
    for (call, agent) in calls {
        let Some(agent) = agent else {
            continue;
        };
        if world
            .get::<ModelRequest>(call)
            .is_some_and(|request| request.connection.is_some())
        {
            continue;
        }
        let needs_model =
            world.get::<ModelRequest>(call).is_some() || world.get::<ToolRequest>(call).is_some();
        if needs_model {
            ready.push((call, agent));
        }
    }
    for (call, agent) in ready {
        let Some(key) = world.get::<AgentKey>(agent).cloned() else {
            continue;
        };
        let purpose = if key.is_work() {
            model::Purpose::Task
        } else {
            model::Purpose::Conversation
        };
        if let Err(error) = chat::ensure_model(world, agent, key.companion(), purpose) {
            world.entity_mut(call).insert(Refusal(error.to_string()));
        }
    }
}

fn prepare(
    mut calls: Query<(&mut ModelRequest, &ForAgent), Added<Admitted>>,
    connections: Query<&rig_ecs::model::Connection>,
) {
    for (mut call, agent) in &mut calls {
        let openai = call
            .connection
            .as_ref()
            .or_else(|| connections.get(agent.0).ok())
            .is_some_and(|connection| connection.spec.reference().starts_with("openai/"));
        if openai {
            call.request_mut().additional_params = Some(json!({"store": false}));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn streams(
    mut deltas: MessageReader<StreamDelta>,
    calls: Query<(&Effect, &CallOf), With<TurnCall>>,
    turns: Query<&Turn>,
    keys: Query<&AgentKey>,
    mut notifier: ResMut<Notifier>,
) {
    for delta in deltas.read() {
        let Ok((_effect, &CallOf(turn_entity))) = calls.get(delta.effect) else {
            continue;
        };
        let Some(agent) = delta.agent else {
            continue;
        };
        let Ok(key) = keys.get(agent) else {
            continue;
        };
        let Ok(turn) = turns.get(turn_entity) else {
            continue;
        };
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        notifier.push(
            Some(key.companion().into()),
            None,
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                companion_id: key.companion().into(),
                stream_id: format!("{}:{}", key.agent_id(), turn.start.get()),
                delta: text.clone(),
                ..Default::default()
            })),
        );
    }
}

fn output_limit(mut calls: Query<&mut Outcome, (With<TurnCall>, ReadyToSettle)>) {
    const MAX_OUTPUT: usize = 128_000;
    for mut outcome in &mut calls {
        if outcome
            .completion()
            .is_some_and(|response| response.text().chars().count() > MAX_OUTPUT)
        {
            *outcome = Outcome::refused("output_limit");
        }
    }
}

fn recalled(
    mut recalled: MessageReader<Recalled>,
    keys: Query<&AgentKey>,
    mut notifier: ResMut<Notifier>,
) {
    for input in recalled.read() {
        let Ok(key) = keys.get(input.agent) else {
            continue;
        };
        notifier.push(
            Some(key.companion().into()),
            None,
            w::notification::Event::Generic(Box::new(w::HostEvent {
                kind: "recalled_input".into(),
                payload: json!({
                    "companion_id": key.companion(),
                    "work_id": key.work_id().unwrap_or_default(),
                    "text": input.text,
                    "reason": input.why,
                })
                .to_string(),
                ..Default::default()
            })),
        );
    }
}

fn changes(
    mut changed: MessageReader<ConversationChanged>,
    keys: Query<&AgentKey>,
    mut notifier: ResMut<Notifier>,
) {
    for change in changed.read() {
        let Ok(key) = keys.get(change.agent) else {
            continue;
        };
        notifier.push(
            Some(key.companion().into()),
            None,
            w::notification::Event::Generic(Box::new(w::HostEvent {
                kind: "conversation_changed".into(),
                payload: json!({
                    "companion_id": key.companion(),
                    "work_id": key.work_id().unwrap_or_default(),
                })
                .to_string(),
                ..Default::default()
            })),
        );
    }
}
