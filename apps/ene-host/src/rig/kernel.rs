//! The headless Rig app: one parent agent per companion, its inbox, turns,
//! calls and session log.

use super::messages;
use super::{Command, Event, Signal};
use crate::domain::{Error, ErrorKind, OperationKind};
use ene_rpc as w;
use rig_core::completion::Message;
use rig_core::conversation::{Author, AuthorKind, MessageId, RequestId};
use rig_ecs::WriteLogs;
use rig_ecs::prelude::*;
use rig_ecs::session_log::{SessionLog, SessionStore};
use rig_memory::DirStore;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// The shortest gap between two frames.
const FRAME: Duration = Duration::from_millis(16);
/// The longest sleep with nothing to do: a retry's backoff and a restored
/// session's deadline are seen within it.
const IDLE: Duration = Duration::from_secs(1);
/// A gap longer than this is counted in full by Bevy's clock, so a retry or
/// timer that passed while the host slept is not stretched.
const MAX_FRAME_GAP: Duration = Duration::from_secs(60);

/// An extension of the app the host registers itself, such as a mind tool.
pub(super) type Configure = Box<dyn FnOnce(&mut App) + Send>;

pub(super) fn run(
    store: DirStore,
    signals: Receiver<Signal>,
    waker: Sender<Signal>,
    events: UnboundedSender<Event>,
    configure: Configure,
) {
    let reporter = events.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_app(store, signals, waker, events, configure)
    }));
    if outcome.is_err() {
        let _ = reporter.send(Event::Fatal {
            reason: "the Rig kernel stopped unexpectedly".into(),
        });
    }
}

fn run_app(
    store: DirStore,
    signals: Receiver<Signal>,
    waker: Sender<Signal>,
    events: UnboundedSender<Event>,
    configure: Configure,
) {
    let waker = Mutex::new(waker);
    let mut app = App::new();
    app.add_plugins(TaskPoolPlugin::default())
        .insert_resource(SessionStore::new(store))
        .insert_resource(Wake::new({
            move || {
                if let Ok(waker) = waker.lock() {
                    let _ = waker.send(Signal::Wake);
                }
            }
        }))
        .add_plugins((AgentPlugin, SessionLogPlugin))
        .insert_resource(Events(events))
        .init_resource::<Pending>()
        .init_resource::<Acks>()
        .init_resource::<Turns>()
        .init_resource::<Emitted>()
        .add_systems(Startup, initialize)
        .add_systems(PreUpdate, apply_commands)
        .add_systems(
            PostUpdate,
            (track_turns, forward_stream, forward_recalled).chain(),
        )
        .add_systems(Last, confirm_deliveries.after(WriteLogs))
        .add_systems(Last, report_log_failure.after(WriteLogs))
        .add_systems(AgentPass, enforce_output_limit.in_set(ReviewSystems::Edit))
        .add_observer(on_turn_settled)
        .add_observer(on_message_started)
        .add_tool(CurrentTime);
    configure(&mut app);
    app.finish();
    if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
        time.set_max_delta(MAX_FRAME_GAP);
    }

    let mut pending: Vec<Command> = Vec::new();
    let mut detached = false;
    loop {
        // Take everything the host sent; one frame applies it all.
        loop {
            match signals.try_recv() {
                Ok(Signal::Command(command)) => pending.push(command),
                Ok(Signal::Wake) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    detached = true;
                    break;
                }
            }
        }
        if detached {
            // The host is gone and nobody reads the events; still stop the
            // turns and write the logs once.
            app.world_mut().write_message(AppExit::Success);
        }
        if !pending.is_empty() {
            let mut queued = app.world_mut().resource_mut::<Pending>();
            queued.0.append(&mut pending);
        }
        let started = Instant::now();
        app.update();
        if app.should_exit().is_some() {
            break;
        }
        if let Some(rest) = FRAME.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
        match signals.recv_timeout(IDLE) {
            Ok(Signal::Command(command)) => pending.push(command),
            Ok(Signal::Wake) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => detached = true,
        }
    }
}

/// Commands wait here for the frame that applies them.
#[derive(Resource, Default)]
struct Pending(Vec<Command>);

/// Deliveries waiting for the frame that accepted them to be written to the
/// session log. Acceptance is returned to the host only after that write, so a
/// crash cannot report a turn started from input that was never saved.
struct PendingAck {
    request_id: String,
    reply: oneshot::Sender<Result<(), Error>>,
}

#[derive(Resource, Default)]
struct Acks {
    pending: Vec<PendingAck>,
    accepted: HashSet<String>,
}

/// What each active turn is, from the API's point of view.
#[derive(Default)]
struct TurnState {
    companion: String,
    stream_id: String,
    offset: u64,
    bytes: usize,
    over_limit: bool,
}

#[derive(Resource, Default)]
struct Turns {
    /// Set once the restored session is in place, so observers do not replay
    /// history as fresh events.
    live: bool,
    by_turn: HashMap<Entity, TurnState>,
    active: HashMap<Entity, Entity>,
}

#[derive(Resource, Default)]
struct Emitted(HashSet<String>);

/// Fills the announced set from the restored session and re-arms the streams
/// of a turn that was still running when the host stopped.
fn initialize(
    agents: Query<(Entity, &AgentId, &Conversation, Option<&ActiveTurn>)>,
    turns: Query<&Turn>,
    mut emitted: ResMut<Emitted>,
    mut state: ResMut<Turns>,
) {
    for (agent, id, conversation, active) in &agents {
        for message in messages::visible_messages(conversation, &id.0) {
            emitted.0.insert(message.id);
        }
        let Some(active) = active else {
            continue;
        };
        let Ok(turn) = turns.get(active.turn()) else {
            continue;
        };
        let (stream_id, _request_id) = stream_of(conversation, turn.start);
        state.active.insert(agent, active.turn());
        state.by_turn.insert(
            active.turn(),
            TurnState {
                companion: id.0.clone(),
                stream_id,
                offset: 0,
                bytes: 0,
                over_limit: false,
            },
        );
    }
    state.live = true;
}

fn agent_entity(world: &mut World, companion: &str) -> Option<Entity> {
    let mut query = world.query_filtered::<(Entity, &AgentId), With<Agent>>();
    query
        .iter(world)
        .find(|(_, id)| id.0 == companion)
        .map(|(entity, _)| entity)
}

fn apply_commands(world: &mut World) {
    let commands = std::mem::take(&mut world.resource_mut::<Pending>().0);
    for command in commands {
        match command {
            Command::Connect {
                companion,
                prompt,
                model,
                mind_tools,
            } => connect(world, companion, prompt, model, mind_tools),
            Command::SetMindPrompt { companion, prompt } => {
                if let Some(entity) = agent_entity(world, &companion) {
                    world.entity_mut(entity).insert(SystemPrompt(prompt));
                }
            }
            Command::Deliver {
                companion,
                text,
                request_id,
                reply,
            } => {
                if let Err(error) = deliver(world, companion, text, request_id.clone()) {
                    let _ = reply.send(Err(error));
                } else {
                    world
                        .resource_mut::<Acks>()
                        .pending
                        .push(PendingAck { request_id, reply });
                }
            }
            Command::HasRequest {
                companion,
                request_id,
                reply,
            } => {
                let result = agent_entity(world, &companion)
                    .and_then(|entity| world.get::<Conversation>(entity))
                    .map(|conversation| messages::has_request(conversation, &request_id))
                    .ok_or_else(|| Error::new(ErrorKind::NotFound));
                let _ = reply.send(result);
            }
            Command::Stop { companion, reason } => stop(world, &companion, reason),
            Command::History {
                companion,
                cursor,
                limit,
                reply,
            } => {
                // A companion with no kernel agent yet has no conversation,
                // which is an empty history rather than a missing companion.
                let result = match agent_entity(world, &companion) {
                    Some(entity) => world
                        .get::<Conversation>(entity)
                        .map(|conversation| {
                            messages::history(conversation, &companion, cursor.as_deref(), limit)
                        })
                        .unwrap_or_else(|| Ok(w::History::default())),
                    None => Ok(w::History::default()),
                };
                let _ = reply.send(result);
            }
            Command::Message {
                companion,
                id,
                reply,
            } => {
                let result = agent_entity(world, &companion)
                    .and_then(|entity| {
                        world
                            .get::<Conversation>(entity)
                            .map(|conversation| messages::message(conversation, &companion, &id))
                    })
                    .ok_or_else(|| Error::new(ErrorKind::NotFound))
                    .and_then(|result| result);
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                world.write_message(AppExit::Success);
                let _ = reply.send(());
            }
        }
    }
}

/// The tools one parent agent may call. Only the bound parent gets the mind
/// tools; any other conversation keeps the harmless current_time tool.
fn tool_access(mind_tools: bool, bridge_bound: bool) -> ToolAccess {
    if mind_tools {
        ToolAccess::Only(vec![
            "current_time".into(),
            "mind_recall".into(),
            "mind_read".into(),
            "mind_update".into(),
        ])
    } else if bridge_bound {
        ToolAccess::Only(vec!["current_time".into()])
    } else {
        ToolAccess::All
    }
}

fn connect(
    world: &mut World,
    companion: String,
    prompt: String,
    model: rig_ecs::model::Connection,
    mind_tools: bool,
) {
    // A kernel without the host-bound bridge (such as a test kernel) must not
    // narrow tool access to tools it does not have.
    let mind_tools = mind_tools
        && world
            .get_resource::<crate::rig::mind::MindBridge>()
            .is_some();
    match agent_entity(world, &companion) {
        Some(entity) => {
            if world.get::<ActiveTurn>(entity).is_some() {
                world.trigger(StopTurn {
                    entity,
                    reason: Some("the model configuration changed".into()),
                });
            }
            let bound = world
                .get_resource::<crate::rig::mind::MindBridge>()
                .is_some_and(|bridge| bridge.bound());
            let access = tool_access(mind_tools, bound);
            world
                .entity_mut(entity)
                .insert((SystemPrompt(prompt), model, access));
        }
        None => {
            let bound = world
                .get_resource::<crate::rig::mind::MindBridge>()
                .is_some();
            let access = tool_access(mind_tools, bound);
            // A fresh app spawns one agent with a random id; it has no
            // conversation and is not a companion. The first companion
            // replaces it rather than leaving a second parent behind.
            let mut agents = world.query_filtered::<(Entity, &Conversation), With<Agent>>();
            let unused = agents
                .iter(world)
                .filter(|(_, conversation)| conversation.entries().next().is_none())
                .map(|(entity, _)| entity)
                .collect::<Vec<_>>();
            for entity in unused {
                world.despawn(entity);
            }
            world.spawn((
                Name::new("agent"),
                Agent,
                AgentId(companion.clone()),
                SystemPrompt(prompt),
                model,
                access,
            ));
        }
    }
    if mind_tools {
        if let Some(bridge) = world.get_resource::<crate::rig::mind::MindBridge>() {
            bridge.bind(companion.clone());
        }
        // Only the bound parent gets the mind tools. Other parent agents keep
        // the harmless current_time tool.
        let mut other =
            world.query_filtered::<(Entity, &AgentId), (With<Agent>, Without<ActiveTurn>)>();
        let strip = other
            .iter(world)
            .filter(|(_, id)| id.0 != companion)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>();
        for entity in strip {
            world.entity_mut(entity).insert(tool_access(false, true));
        }
    }
}

fn deliver(
    world: &mut World,
    companion: String,
    text: String,
    request_id: String,
) -> Result<(), Error> {
    let entity = agent_entity(world, &companion).ok_or_else(|| Error::new(ErrorKind::NotFound))?;
    if world.get::<Connection>(entity).is_none() {
        return Err(Error {
            kind: ErrorKind::StateConflict,
            reason: Some("model_unset"),
        });
    }
    let author = Author {
        kind: AuthorKind::User,
        request: Some(RequestId(request_id)),
        ..Default::default()
    };
    world.spawn((
        Incoming::new(text, DeliveryMode::Steer).with_author(author),
        ForAgent(entity),
    ));
    Ok(())
}

fn stop(world: &mut World, companion: &str, reason: Option<String>) {
    let Some(entity) = agent_entity(world, companion) else {
        return;
    };
    world.trigger(StopTurn { entity, reason });
}

/// The stream id and request id of a turn: the request entry it answers, or
/// the first entry it adds.
fn stream_of(conversation: &Conversation, start: MessageId) -> (String, Option<String>) {
    match messages::request_entry(conversation, start) {
        Some(entry) => (
            entry.id.to_string(),
            entry
                .author
                .and_then(|author| author.request.as_ref())
                .map(|request| request.0.clone()),
        ),
        None => (start.to_string(), None),
    }
}

/// Registers turns that started this frame so their streams and inputs are
/// known before the frame's deltas are read.
fn track_turns(
    agents: Query<(Entity, &AgentId, &Conversation, &ActiveTurn)>,
    turns: Query<&Turn>,
    mut state: ResMut<Turns>,
    mut emitted: ResMut<Emitted>,
    events: Res<Events>,
) {
    if !state.live {
        return;
    }
    for (agent, id, conversation, active) in &agents {
        if state.active.get(&agent) == Some(&active.turn()) {
            continue;
        }
        let Ok(turn) = turns.get(active.turn()) else {
            continue;
        };
        let (stream_id, request_id) = stream_of(conversation, turn.start);
        let messages = take_new(conversation, &id.0, &mut emitted);
        state.active.insert(agent, active.turn());
        state.by_turn.insert(
            active.turn(),
            TurnState {
                companion: id.0.clone(),
                stream_id: stream_id.clone(),
                offset: 0,
                bytes: 0,
                over_limit: false,
            },
        );
        events.send(Event::TurnStarted {
            companion: id.0.clone(),
            stream_id,
            request_id,
            messages,
        });
    }
}

fn on_turn_settled(
    settled: On<Add<Settled>>,
    turns: Query<(&TurnOutcome, &ForAgent, &Turn)>,
    agents: Query<(Entity, &AgentId, &Conversation)>,
    mut state: ResMut<Turns>,
    mut emitted: ResMut<Emitted>,
    events: Res<Events>,
) {
    if !state.live {
        return;
    }
    let Ok((outcome, agent, turn)) = turns.get(settled.entity) else {
        return;
    };
    let Ok((agent_entity, id, conversation)) = agents.get(agent.0) else {
        return;
    };
    let turn_state = state.by_turn.remove(&settled.entity);
    state.active.remove(&agent_entity);
    let (stream_id, request_id) = stream_of(conversation, turn.start);
    if turn_state.is_none() {
        // A turn that both started and ended between two frames still tells
        // its input before its end.
        events.send(Event::TurnStarted {
            companion: id.0.clone(),
            stream_id: stream_id.clone(),
            request_id: request_id.clone(),
            messages: take_new(conversation, &id.0, &mut emitted),
        });
    }
    let messages = take_new(conversation, &id.0, &mut emitted);
    let over_limit = turn_state.as_ref().is_some_and(|state| state.over_limit);
    let (state_name, failure) = match &outcome {
        TurnOutcome::Answered(_) => ("answered", None),
        TurnOutcome::Failed(why) => {
            let (code, message) = if why == "output_limit" {
                ("output_limit", "返答が出力上限を超えたため破棄しました")
            } else {
                ("generation_failed", why.as_str())
            };
            ("failed", Some(messages::failure(code, message)))
        }
        TurnOutcome::Stopped(reason) => (
            "stopped",
            Some(messages::failure(
                if over_limit {
                    "output_limit"
                } else {
                    "cancelled"
                },
                reason
                    .as_deref()
                    .unwrap_or("停止操作により生成を停止しました"),
            )),
        ),
    };
    let message_id = matches!(outcome, TurnOutcome::Answered(_) | TurnOutcome::Stopped(_))
        .then(|| messages::reply_entry(conversation, turn.start))
        .flatten();
    events.send(Event::TurnFinished {
        companion: id.0.clone(),
        stream_id,
        request_id,
        state: state_name,
        message_id,
        messages,
        failure,
    });
}

/// Reads the reply stream of a running turn and asks for it to stop before
/// the conversation output limit is exceeded.
fn forward_stream(
    mut streamed: MessageReader<StreamDelta>,
    turn_calls: Query<(), With<TurnCall>>,
    mut state: ResMut<Turns>,
    events: Res<Events>,
    mut commands: Commands,
) {
    for delta in streamed.read() {
        // Only the turn's own model call streams to the viewer; a side call
        // or a subagent's text is not this reply.
        if turn_calls.get(delta.effect).is_err() {
            continue;
        }
        let Some(agent) = delta.agent else {
            continue;
        };
        let Some(turn) = state.active.get(&agent).copied() else {
            continue;
        };
        let Some(turn_state) = state.by_turn.get_mut(&turn) else {
            continue;
        };
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        if turn_state.over_limit {
            continue;
        }
        if turn_state.bytes.saturating_add(text.len())
            > OperationKind::Conversation.spec().output_bytes
        {
            turn_state.over_limit = true;
            commands.queue(move |world: &mut World| {
                world.trigger(StopTurn {
                    entity: agent,
                    reason: Some("output_limit".into()),
                });
            });
            continue;
        }
        turn_state.bytes += text.len();
        let offset = turn_state.offset;
        turn_state.offset += text.chars().count() as u64;
        events.send(Event::Delta {
            companion: turn_state.companion.clone(),
            stream_id: turn_state.stream_id.clone(),
            offset,
            delta: text.clone(),
        });
    }
}

fn take_new(
    conversation: &Conversation,
    companion: &str,
    emitted: &mut Emitted,
) -> Vec<w::Message> {
    messages::visible_messages(conversation, companion)
        .into_iter()
        .filter(|message| emitted.0.insert(message.id.clone()))
        .collect()
}

/// Confirms each delivery only once the frame's log write succeeded and the
/// kernel started the message. A pending delivery is retried next frame.
fn confirm_deliveries(log: Res<SessionLog>, mut acks: ResMut<Acks>) {
    if let Some(failure) = log.failure() {
        for ack in acks.pending.drain(..) {
            let _ = ack.reply.send(Err(Error {
                kind: ErrorKind::StorageFailure,
                reason: Some("session_log_failed"),
            }));
        }
        acks.accepted.clear();
        let _ = failure;
        return;
    }
    let Acks {
        pending: waiting,
        accepted,
    } = &mut *acks;
    let mut pending = Vec::with_capacity(waiting.len());
    for ack in waiting.drain(..) {
        if accepted.remove(&ack.request_id) {
            let _ = ack.reply.send(Ok(()));
        } else {
            pending.push(ack);
        }
    }
    *waiting = pending;
}

/// Notes the request id of each accepted message so its delivery can be
/// confirmed after the log write.
fn on_message_started(
    started: On<Add<Started>>,
    incoming: Query<&Incoming>,
    mut acks: ResMut<Acks>,
) {
    let Ok(message) = incoming.get(started.entity) else {
        return;
    };
    if let Some(request) = &message.author.request {
        acks.accepted.insert(request.0.clone());
    }
}

/// Reports text a failed turn returned to the client, so its input is not
/// lost when the kernel removes it from the conversation.
fn forward_recalled(
    mut recalled: MessageReader<Recalled>,
    agents: Query<&AgentId, With<Agent>>,
    events: Res<Events>,
) {
    for message in recalled.read() {
        let Ok(id) = agents.get(message.agent) else {
            continue;
        };
        events.send(Event::Recalled {
            companion: id.0.clone(),
            text: message.text.clone(),
            why: message.why.clone(),
        });
    }
}

/// Refuses a model reply that exceeds the conversation's output budget before
/// it can be committed. Streaming deltas are stopped earlier; this catches a
/// single reply event that arrived whole.
fn enforce_output_limit(mut calls: Query<&mut Outcome, (With<TurnCall>, ReadyToSettle)>) {
    for mut outcome in &mut calls {
        let Some(response) = outcome.completion() else {
            continue;
        };
        let text = response
            .message()
            .map(|message| match message {
                Message::Assistant(reply) => reply.text(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if text.len() > OperationKind::Conversation.spec().output_bytes {
            *outcome = Outcome::refused("output_limit");
        }
    }
}

/// Reports a session log that stopped writing once, so the host stops
/// accepting work instead of reporting saves it does not have.
fn report_log_failure(log: Res<SessionLog>, mut reported: Local<bool>, events: Res<Events>) {
    if *reported {
        return;
    }
    let Some(failure) = log.failure() else {
        return;
    };
    *reported = true;
    events.send(Event::StorageFailed {
        reason: failure.chars().take(1024).collect(),
    });
}

/// The one harmless host tool step 1 needs: it answers with the current UTC
/// time and changes nothing.
struct CurrentTime;

#[derive(serde::Deserialize)]
struct NoArguments {}

impl Tool for CurrentTime {
    const NAME: &'static str = "current_time";
    type Args = NoArguments;
    type Output = String;
    type Error = std::convert::Infallible;

    fn description(&self) -> String {
        "Returns the current UTC date and time in RFC 3339 format. Use it when the user asks what time or date it is now.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: NoArguments) -> Result<Self::Output, Self::Error> {
        Ok(time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default())
    }
}

/// The kernel writes events through this resource so systems need no channel
/// parameter of their own.
#[derive(Resource)]
struct Events(UnboundedSender<Event>);

impl Events {
    fn send(&self, event: Event) {
        let _ = self.0.send(event);
    }
}
