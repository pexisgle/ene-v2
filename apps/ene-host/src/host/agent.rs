//! Agent identity, prompts and the Rig kernel extension points.
use super::{
    body, chat, client, compaction,
    db::{Result, Store},
    error::HostError,
    mind,
    model::{self, Binding, Refusal},
    notify::Notifier,
    resources::{ApprovalHolds, CompactionQueue, ConsolidationQueue, HostOptions, HostState},
    settings, tools, wire, work,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::{completion::Message, conversation::Author};
use rig_ecs::{WriteLogs, prelude::*};
use rig_memory::recent::TokenBudget;
use rig_memory::{HeuristicTokenCounter, TokenCounter, recent};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

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
            .add_tool(tools::current_time_tool())
            .add_observer(turn_input)
            .add_observer(turn_started)
            .add_observer(turn_settled)
            .add_observer(follow_successor::<Binding>)
            .add_observer(follow_successor::<mind::Batch>)
            .add_observer(follow_successor::<compaction::SummaryJob>)
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
            .add_systems(AgentPass, fit.in_set(PrepareSystems::Fit))
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(AgentPass, streams.in_set(Stage::Review))
            .add_systems(
                AgentPass,
                (output_limit, compaction::apply, mind::apply_consolidation)
                    .in_set(ReviewSystems::Edit),
            )
            .add_systems(PostUpdate, recalled)
            .add_systems(Last, changes.after(WriteLogs))
            .add_systems(Last, work::finalize_reports.after(changes))
            .add_systems(Last, settings::expire_orphan_holds);
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
fn fit(
    mut calls: Query<(Entity, &mut ModelRequest, &ForAgent, &Adapted), Added<Admitted>>,
    keys: Query<&AgentKey>,
    store: Res<Store>,
    options: Res<HostOptions>,
    mut queues: ResMut<ConsolidationQueue>,
    mut compaction: ResMut<CompactionQueue>,
    mut notifier: ResMut<Notifier>,
    mut reported: Local<HashSet<(Entity, &'static str)>>,
    mut commands: Commands,
) {
    for (call_entity, mut call, agent, adapted) in &mut calls {
        let key = keys.get(agent.0).ok();
        let spec = &adapted.0.spec;
        if let Some(max) = spec.max_output_tokens {
            call.request_mut().max_tokens = Some(u64::from(max));
        }
        let Some(window) = spec.context_window else {
            if reported.insert((agent.0, "window"))
                && let Some(key) = key
            {
                notifier.notice(
                    key.companion(),
                    "context_window_unknown",
                    "the model's context length is unknown; the whole conversation is sent",
                );
            }
            continue;
        };
        let counter = HeuristicTokenCounter::default();
        let reserve = spec
            .max_output_tokens
            .map_or(0usize, |tokens| tokens as usize)
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
        let budget = (window as usize).saturating_sub(reserve);
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let protected = usize::from(
            history
                .entries()
                .next()
                .and_then(|entry| chat::entry_text(entry.message))
                .is_some_and(|text| text.starts_with("<summary>")),
        );
        let protected_tokens = history
            .messages()
            .first()
            .filter(|_| protected == 1)
            .map_or(0, |message| counter.count(message));
        let budget = budget.saturating_sub(protected_tokens);
        let Some(last) = history.messages().last() else {
            continue;
        };
        if counter.count(last) > budget {
            if let Some(key) = key {
                notifier.notice(
                    key.companion(),
                    "context_exceeded",
                    "the latest message does not fit the model's context window",
                );
            }
            commands
                .entity(call_entity)
                .insert(Refusal("context_exceeded".into()));
            continue;
        }
        let kept = recent::view(&history.messages()[protected..], &TokenBudget::new(budget));
        let kept_len = kept.len();
        let len = history.messages().len();
        let start = len
            .saturating_sub(kept_len)
            .max(protected)
            .min(len.saturating_sub(1));
        if start <= protected {
            continue;
        }
        let Some(key) = key else {
            continue;
        };
        let cursor = store
            .consolidation(key.companion())
            .ok()
            .flatten()
            .unwrap_or_default();
        let dropped: Vec<(u64, bool)> = history
            .entries()
            .skip(protected)
            .take(start - protected)
            .map(|entry| (entry.id.get(), entry.author.is_none()))
            .collect();
        let unconsolidated = dropped
            .iter()
            .any(|(id, own)| *own && *id > cursor.last_sequence);
        history.splice(protected..start, [], Author::plugin("ene.window"));
        if unconsolidated {
            queues.0.insert(key.companion().into());
            if reported.insert((agent.0, "truncated")) {
                notifier.notice(
                    key.companion(),
                    "window_truncated_unconsolidated",
                    "the window dropped messages that are not consolidated into memory yet",
                );
            }
        }
        if (kept_len as f64) / (window as f64) > options.compaction.threshold {
            compaction.0.insert(key.companion().into());
        }
    }
}

fn gate(world: &mut World) {
    let calls: Vec<Entity> = world
        .query_filtered::<Entity, ReadyToStart>()
        .iter(world)
        .collect();
    for entity in calls {
        let ready = world.resource::<HostState>().ready().err();
        let needs_binding = world.get::<ModelRequest>(entity).is_some()
            || world.get::<ToolRequest>(entity).is_some();
        let permitted = if needs_binding {
            world
                .get::<Binding>(entity)
                .or_else(|| {
                    world
                        .get::<ForAgent>(entity)
                        .and_then(|agent| world.get::<Binding>(agent.0))
                })
                .is_some_and(|binding| binding.permit.load(std::sync::atomic::Ordering::Acquire))
        } else {
            true
        };
        let tool_request = world.get::<ToolRequest>(entity).map(|request| {
            (
                request.call.function.name.to_string(),
                serde_json::to_string(&request.call.function.arguments).unwrap_or_default(),
            )
        });
        let policy = match tool_request.as_ref() {
            Some((name, args)) => match policy_refusal(world, entity, name, args) {
                Ok(Policy::Allow) => Policy::Allow,
                Ok(Policy::Hold { reason }) => Policy::Hold { reason },
                Ok(Policy::Refuse(reason)) => Policy::Refuse(reason),
                Err(error) => Policy::Refuse(format!("policy unavailable: {error}")),
            },
            None => Policy::Allow,
        };
        let hold = matches!(&policy, Policy::Hold { .. });
        if hold {
            continue;
        }
        let policy = match policy {
            Policy::Refuse(reason) => Some(reason),
            _ => None,
        };
        let refusal = if let Some(policy) = policy {
            Some(policy)
        } else if let Some(refusal) = world.get::<Refusal>(entity) {
            Some(format!("model unavailable: {}", refusal.0))
        } else if permitted {
            None
        } else {
            Some("model permission changed".into())
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

enum Policy {
    Allow,
    Refuse(String),
    Hold { reason: String },
}

fn ensure_hold(world: &mut World, request_id: &str, call: Entity) {
    let existing = world.resource::<ApprovalHolds>().0.get(request_id).copied();
    if let Some(hold) = existing {
        let live = world
            .get::<HoldOf>(hold)
            .is_some_and(|HoldOf(held)| *held == call);
        if live {
            return;
        }
        world.resource_mut::<ApprovalHolds>().0.remove(request_id);
    }
    let hold = world.spawn(HoldOf(call)).id();
    world
        .resource_mut::<ApprovalHolds>()
        .0
        .insert(request_id.to_owned(), hold);
}

fn policy_refusal(world: &mut World, entity: Entity, name: &str, args: &str) -> Result<Policy> {
    let store = world.resource::<Store>().clone();
    let rules = store.policy_rules()?;
    let matches = |rule: &w::PolicyRule| {
        (rule.tool_name.is_empty() || rule.tool_name == "*" || rule.tool_name == name)
            && (rule.pattern.is_empty() || args.contains(&rule.pattern))
    };
    if rules
        .iter()
        .any(|rule| rule.effect == "deny" && matches(rule))
    {
        return Ok(Policy::Refuse("policy denied".into()));
    }
    if !rules
        .iter()
        .any(|rule| rule.effect == "confirm" && matches(rule))
    {
        return Ok(Policy::Allow);
    }
    let hash = crate::host::db::digest(format!("{name}:{args}").as_bytes());
    let agent = world
        .get::<ForAgent>(entity)
        .map(|agent| agent.0)
        .ok_or_else(|| HostError::permission("unbound tool call"))?;
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    let session_id = key.agent_id();
    let run_id = world
        .get::<CallOf>(entity)
        .and_then(|CallOf(turn)| world.get::<Turn>(*turn))
        .map(|turn| format!("{}:{}", session_id, turn.start.get()))
        .unwrap_or_default();
    if let Some(existing) = store.approval_for(name, hash.as_bytes(), &session_id, &run_id)? {
        match existing.status.as_str() {
            "approved" => {
                if store.consume_approval(&existing.request_id)? {
                    return Ok(Policy::Allow);
                }
                return Ok(Policy::Refuse("approval was already consumed".into()));
            }
            "pending" => {
                ensure_hold(world, &existing.request_id, entity);
                return Ok(Policy::Hold {
                    reason: "approval pending".into(),
                });
            }
            "denied" => return Ok(Policy::Refuse("approval denied".into())),
            _ => {}
        }
    }
    let request = w::ApprovalRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id,
        run_id,
        action_payload_hash: hash.as_bytes().to_vec(),
        tool_name: name.into(),
        normalized_arguments: args.into(),
        status: "pending".into(),
        created_at: crate::host::db::now(),
        ..Default::default()
    };
    store.save_approval(&request)?;
    ensure_hold(world, &request.request_id, entity);
    world.resource_mut::<Notifier>().push(
        Some(key.companion().into()),
        None,
        w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
            request: request.clone().into(),
            ..Default::default()
        })),
    );
    Ok(Policy::Hold {
        reason: "approval required".into(),
    })
}

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
