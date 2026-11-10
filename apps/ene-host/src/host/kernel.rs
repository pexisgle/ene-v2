//! Rig ECS schedule pipeline: Prepare, Gate, Review, SessionLog commits and tool dispatch.
use super::{
    Host, Owner, chat, client,
    db::{self, is_storage, permission},
    fail, mind, model, work,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::{
    completion::Message,
    conversation::{Author, MessageId},
    tool::ToolResult,
};
use rig_derive::rig_tool;
use rig_ecs::{
    WriteLogs,
    prelude::*,
    session_log::{ConversationChanged, SessionLog},
};
use rig_memory::{HeuristicTokenCounter, TokenCounter, recent::TokenBudget};
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub(super) const PARENT_TOOLS: &[&str] = &[
    "current_time",
    "mind_recall",
    "mind_read",
    "mind_update",
    "start_work",
    "send_work",
    "stop_work",
    "list_work",
    "device_request",
];
pub(super) const WORK_TOOLS: &[&str] = &["current_time", "report", "device_request"];

/// Get the current Unix time in milliseconds.
#[rig_tool]
fn current_time() -> Result<String, std::convert::Infallible> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
        .to_string())
}

pub(super) struct KernelPlugin;

impl Plugin for KernelPlugin {
    fn build(&self, app: &mut App) {
        app.add_tool(CurrentTime)
            .add_observer(accepted)
            .add_observer(turn_input)
            .add_observer(turn_started)
            .add_observer(turn_settled)
            .add_observer(follow_successor::<mind::Batch>)
            .add_observer(follow_successor::<model::Binding>)
            .add_systems(Update, mind::start_consolidation.before(AgentLoop))
            .add_systems(AgentPass, bindings.in_set(PrepareSystems::Compose))
            .add_systems(AgentPass, prepare.in_set(PrepareSystems::Edit))
            .add_systems(AgentPass, fit.in_set(PrepareSystems::Fit))
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(AgentPass, streams.in_set(Stage::Review))
            .add_systems(
                AgentPass,
                (output_limit, mind::apply_consolidation, mind::apply_summary)
                    .in_set(ReviewSystems::Edit),
            )
            .add_systems(PostUpdate, recalled)
            .add_systems(Last, changes.after(WriteLogs));
        mind::register_tools(app);
        work::register_tools(app);
        client::register_tools(app);
    }
}

pub(crate) fn run_tool<A: Clone + Send + Sync + 'static>(
    handler: fn(&mut World, Entity, &str, A) -> db::Result<String>,
) -> impl Fn(On<ToolCalled<A>>, Commands) {
    move |called: On<ToolCalled<A>>, mut commands: Commands| {
        let ToolCalled {
            call,
            agent,
            effect,
            args,
            ..
        } = called.event().clone();
        commands.queue(move |world: &mut World| {
            let result = handler(world, agent, &effect.to_string(), args);
            answer(world, call, result);
        });
    }
}

pub(crate) fn answer(world: &mut World, call: Entity, result: db::Result<String>) {
    let outcome = match result {
        Ok(text) => Outcome::from(ToolResult::success(text.into())),
        Err(error) => {
            if is_storage(&error) {
                fail(world, error.to_string());
            }
            Outcome::refused(error.to_string())
        }
    };
    if let Some(mut decision) = world.get_mut::<Decision>(call) {
        decision.decide(Decider::plugin("ene-host"), outcome);
    }
}

pub(crate) fn parent_owner(world: &World, agent: Entity) -> db::Result<Owner> {
    let owner = world
        .get::<Owner>(agent)
        .cloned()
        .ok_or_else(|| permission("unbound agent"))?;
    if owner.is_work() {
        return Err(permission("not available to work agents"));
    }
    Ok(owner)
}

fn accepted(
    started: On<Add<Started>>,
    inputs: Query<(&Incoming, &ForAgent)>,
    mut conversations: Query<&mut Conversation>,
) {
    let Ok((input, agent)) = inputs.get(started.entity) else {
        return;
    };
    let Some(request) = &input.author.request else {
        return;
    };
    if let Ok(mut conversation) = conversations.get_mut(agent.0) {
        conversation.add_aside(Aside::new("ene.accepted", Author::rig(), json!(request.0)));
    }
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
    let input = input_of(&conversation, turn.start);
    if !input.is_empty() {
        conversation.add_aside(Aside::new(
            "ene.turn_input",
            Author::rig(),
            json!({"start": turn.start.get(), "input_id": input}),
        ));
    }
}

pub(crate) fn input_of(conversation: &Conversation, start: MessageId) -> String {
    conversation
        .request(start)
        .and_then(|entry| entry.author)
        .and_then(|author| author.request.as_ref())
        .map(|request| request.0.clone())
        .or_else(|| {
            conversation.asides().find_map(|(_, aside)| {
                (aside.kind == "ene.turn_input"
                    && aside.value["start"].as_u64() == Some(start.get()))
                .then(|| aside.value["input_id"].as_str().map(str::to_owned))
                .flatten()
            })
        })
        .unwrap_or_default()
}

fn turn_started(started: On<Add<TurnOf>>, turns: Query<&TurnOf>, mut commands: Commands) {
    let Ok(&TurnOf(agent)) = turns.get(started.entity) else {
        return;
    };
    let turn_entity = started.entity;
    commands.queue(move |world: &mut World| {
        let Some(owner) = world.get::<Owner>(agent).cloned() else {
            return;
        };
        let Some(turn) = world.get::<Turn>(turn_entity) else {
            return;
        };
        let request = world
            .get::<Conversation>(agent)
            .map(|c| input_of(c, turn.start))
            .unwrap_or_default();
        let event = w::TurnEvent {
            companion_id: owner.companion().into(),
            work_id: owner.work().into(),
            stream_id: format!("{}:{}", owner.conversation_id(), turn.start.get()),
            state: "streaming".into(),
            request_id: (!request.is_empty()).then_some(request),
            ..Default::default()
        };
        world.resource_mut::<Host>().notify(
            None,
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
        let Some(owner) = world.get::<Owner>(agent).cloned() else {
            return;
        };
        if !owner.is_work() {
            world
                .resource_mut::<Host>()
                .consolidation_queue
                .insert(owner.companion().into());
        } else if let Err(error) = work::report_work(world, agent)
            && is_storage(&error)
        {
            fail(world, error.to_string());
        }
        let Some(conversation) = world.get::<Conversation>(agent) else {
            return;
        };
        let Some((_, record)) = conversation.turns().next_back() else {
            return;
        };
        let start = record.start;
        let (state, failure) = chat::turn_outcome(record.end.as_ref());
        let message_id = chat::last_assistant(conversation, start).map(|(id, _)| id.to_string());
        let request = input_of(conversation, start);
        let event = w::TurnEvent {
            companion_id: owner.companion().into(),
            work_id: owner.work().into(),
            stream_id: format!("{}:{}", owner.conversation_id(), start.get()),
            state: state.into(),
            request_id: (!request.is_empty()).then_some(request),
            message_id,
            failure: failure.into(),
            ..Default::default()
        };
        world.resource_mut::<Host>().notify(
            None,
            None,
            w::notification::Event::TurnEvent(Box::new(event)),
        );
    });
}

/// Binds the model of every agent whose call is about to run.
///
/// A turn can start with no RPC request behind it: a work report, a
/// scheduled wake, a tool's delivery. Binding here, on the one path every
/// model call takes, is what makes those turns possible; a binding that was
/// revoked or went stale is replaced before the call is routed to a model.
fn bindings(world: &mut World) {
    let mut calls = Vec::new();
    for (call, agent) in world
        .query_filtered::<(Entity, Option<&ForAgent>), ReadyToStart>()
        .iter(world)
    {
        let Some(&ForAgent(agent)) = agent else {
            continue;
        };
        // A side call brought its own connection, and its own purpose.
        if world
            .get::<ModelRequest>(call)
            .is_some_and(|request| request.connection.is_some())
        {
            continue;
        }
        let needs_model =
            world.get::<ModelRequest>(call).is_some() || world.get::<ToolRequest>(call).is_some();
        if needs_model {
            calls.push((call, agent));
        }
    }
    for (call, agent) in calls {
        let Some(owner) = world.get::<Owner>(agent).cloned() else {
            continue;
        };
        let purpose = if owner.is_work() {
            model::Purpose::Task
        } else {
            model::Purpose::Conversation
        };
        if let Err(error) = chat::ensure(world, agent, owner.companion(), purpose) {
            world
                .entity_mut(call)
                .insert(model::Refused(error.to_string()));
        }
    }
}

/// OpenAI keeps a stored copy of every request; this host keeps its own in
/// the session log, so the provider's copy is turned off.
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

/// Fits each model call to the window of the model it goes to.
///
/// `ModelRequest::history` is the request's own view of the conversation, so
/// cutting it changes neither the session log, `GetHistory` nor the
/// consolidation cursor. What the window is unknown for is left whole and
/// reported once, never guessed.
fn fit(
    mut calls: Query<(&mut ModelRequest, &ForAgent, &Adapted), Added<Admitted>>,
    owners: Query<&Owner>,
    summaries: Query<&mind::Summary>,
    jobs: Query<&mind::SummaryJob>,
    mut host: ResMut<Host>,
    mut commands: Commands,
    mut reported: Local<HashSet<(Entity, &'static str)>>,
) {
    for (mut call, agent, adapted) in &mut calls {
        let spec = &adapted.0.spec;
        if let Some(max) = spec.max_output_tokens {
            call.request_mut().max_tokens = Some(u64::from(max));
        }
        let Some(window) = spec.context_window else {
            if reported.insert((agent.0, "window"))
                && let Ok(owner) = owners.get(agent.0)
            {
                super::notice(
                    &mut host,
                    owner.companion(),
                    "context_window_unknown",
                    "the model's context length is unknown; the whole conversation is sent",
                );
            }
            continue;
        };
        let Ok(owner) = owners.get(agent.0) else {
            continue;
        };
        let summary = summaries.get(agent.0).ok();
        let counter = HeuristicTokenCounter::default();
        let reserve = spec
            .max_output_tokens
            .map_or(0usize, |tokens| tokens as usize)
            // The summary rides in front, so the window leaves it room.
            + summary
                .and_then(mind::Summary::text)
                .map_or(0, |text| counter.count(&Message::user(text)))
            + call
                .prompt
                .as_ref()
                .map_or(0, |prompt| counter.count(&Message::system(prompt.text())))
            + call
                .request
                .tools
                .iter()
                .map(|tool| counter.count_tool(tool))
                .sum::<usize>();
        // ponytail: a byte-ratio estimate, not the provider's tokenizer; it
        // errs towards a larger window (saturating), never a shorter one.
        let budget = (window as usize).saturating_sub(reserve);
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let kept = rig_memory::recent::view(history.messages(), &TokenBudget::new(budget));
        let len = history.messages().len();
        // The newest message is the question being asked: it always stays.
        let start = (len.saturating_sub(kept.len())).min(len.saturating_sub(1));
        if start == 0 {
            continue;
        }
        let cursor = host
            .db
            .get::<mind::Consolidation>(&format!("memory-cursor:{}", owner.companion()))
            .ok()
            .flatten()
            .unwrap_or_default();
        let dropped: Vec<_> = history.entries().take(start).collect();
        let through = dropped.last().map_or(0, |entry| entry.id.get());
        let unconsolidated = dropped
            .iter()
            .filter(|entry| entry.author.is_none())
            .any(|entry| entry.id.get() > cursor.last_sequence);
        let fresh: Vec<_> = dropped
            .iter()
            .filter(|entry| summary.is_none_or(|summary| entry.id.get() > summary.through))
            .map(|entry| history.messages()[entry.index].clone())
            .collect();
        // Cut first, so the summary goes before what was kept.
        history.splice(0..start, [], Author::plugin("ene.window"));
        mind::inject_summary(history, summary);
        if unconsolidated {
            if reported.insert((agent.0, "truncated")) {
                host.consolidation_queue.insert(owner.companion().into());
                super::notice(
                    &mut host,
                    owner.companion(),
                    "window_truncated_unconsolidated",
                    "the window dropped messages that are not consolidated into memory yet",
                );
            }
            continue;
        }
        let pending = jobs.iter().any(|job| job.agent == agent.0);
        mind::start_summary(
            &mut commands,
            &mut host,
            agent.0,
            owner.companion(),
            summary,
            &fresh,
            through,
            pending,
        );
    }
}

fn gate(world: &mut World) {
    let calls: Vec<Entity> = world
        .query_filtered::<Entity, ReadyToStart>()
        .iter(world)
        .collect();
    for entity in calls {
        let ready = world.resource::<Host>().ready().err();
        let needs_binding = world.get::<ModelRequest>(entity).is_some()
            || world.get::<ToolRequest>(entity).is_some();
        let permitted = if needs_binding {
            world
                .get::<model::Binding>(entity)
                .or_else(|| {
                    world
                        .get::<ForAgent>(entity)
                        .and_then(|agent| world.get::<model::Binding>(agent.0))
                })
                .is_some_and(|binding| binding.permit.load(std::sync::atomic::Ordering::Acquire))
        } else {
            true
        };
        let refusal = if permitted {
            None
        } else {
            Some(
                world
                    .get::<model::Refused>(entity)
                    .map(|refused| format!("model unavailable: {}", refused.0))
                    .unwrap_or_else(|| "model permission changed".into()),
            )
        };
        let Some(mut decision) = world.get_mut::<Decision>(entity) else {
            continue;
        };
        if decision.is_decided() {
            continue;
        }
        if let Some(error) = ready {
            decision.decide(
                Decider::plugin("ene-host"),
                Outcome::refused(error.to_string()),
            );
        } else if let Some(why) = refusal {
            tracing::warn!(call = ?entity, %why, "a call was refused");
            decision.decide(Decider::plugin("ene-host"), Outcome::refused(why));
        }
    }
}

fn streams(
    mut deltas: MessageReader<StreamDelta>,
    calls: Query<(&Effect, &CallOf), With<TurnCall>>,
    turns: Query<&Turn>,
    owners: Query<&Owner>,
    mut offsets: Local<HashMap<Entity, u64>>,
    mut host: ResMut<Host>,
) {
    offsets.retain(|call, _| calls.contains(*call));
    for delta in deltas.read() {
        // Only a turn's own call streams into its conversation. A plugin's
        // side call, such as consolidation, is `ForAgent` without `TurnCall`
        // and must never reach a stream, even while its agent has a turn.
        let Ok((effect, &CallOf(turn_entity))) = calls.get(delta.effect) else {
            continue;
        };
        let Some(agent) = delta.agent else {
            continue;
        };
        let Ok(owner) = owners.get(agent) else {
            continue;
        };
        let Ok(turn) = turns.get(turn_entity) else {
            continue;
        };
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        let offset = offsets.entry(delta.effect).or_default();
        host.notify(
            Some(owner.companion().into()),
            None,
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                stream_id: format!("{}:{}", owner.conversation_id(), turn.start.get()),
                offset: *offset,
                delta: text.clone(),
                call_id: effect.id().to_string(),
                ..Default::default()
            })),
        );
        *offset += text.chars().count() as u64;
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

fn recalled(mut recalled: MessageReader<Recalled>, owners: Query<&Owner>, mut host: ResMut<Host>) {
    for input in recalled.read() {
        let Ok(owner) = owners.get(input.agent) else {
            continue;
        };
        host.notify(
            Some(owner.companion().into()),
            None,
            w::notification::Event::RecalledInput(Box::new(w::RecalledInput {
                companion_id: owner.companion().into(),
                work_id: owner.work().into(),
                text: input.text.clone(),
                reason: input.why.clone(),
                ..Default::default()
            })),
        );
    }
}

fn changes(
    mut changed: MessageReader<ConversationChanged>,
    attached: Query<Entity, Added<Owner>>,
    owners: Query<(Entity, &Owner)>,
    mut host: ResMut<Host>,
    log: Res<SessionLog>,
    mut commands: Commands,
) {
    if host.failed.is_some() || log.failure().is_some() {
        return;
    }
    if let Err(error) = client::sweep_closed(&mut host)
        && is_storage(&error)
    {
        commands.queue(move |world: &mut World| fail(world, error.to_string()));
    }
    let mut companions = HashSet::new();
    for change in changed.read() {
        if let Ok((_, owner)) = owners.get(change.agent) {
            companions.insert(owner.companion().to_owned());
            host.notify(
                Some(owner.companion().into()),
                None,
                w::notification::Event::ConversationChanged(Box::new(w::ConversationChanged {
                    companion_id: owner.companion().into(),
                    work_id: owner.work().into(),
                    ..Default::default()
                })),
            );
        }
    }
    for agent in &attached {
        if let Ok((_, owner)) = owners.get(agent) {
            companions.insert(owner.companion().to_owned());
        }
    }
    if !host.closing && !companions.is_empty() {
        for (worker, owner) in &owners {
            if owner.is_work() && companions.contains(owner.companion()) {
                commands.queue(move |world: &mut World| {
                    if world.resource::<Host>().ready().is_ok()
                        && let Err(error) = work::report_work(world, worker)
                        && is_storage(&error)
                    {
                        fail(world, error.to_string());
                    }
                });
            }
        }
    }
}
