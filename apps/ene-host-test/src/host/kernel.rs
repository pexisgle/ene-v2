//! Extensions on Rig's public Prepare/Gate/stream/session-log API.
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::Ordering,
};

use connectrpc::ConnectError;
use rig_core::completion::{Message, Section};
use rig_ecs::{WriteLogs, prelude::*, session_log::SessionLog};
use rig_memory::{HeuristicTokenCounter, TokenCounter, recent::TokenBudget};
use uuid::Uuid;

use super::{
    Binding, Host, Owner, conversation, fail,
    store::{Result, is_storage},
    tools::{GUARDED_UPDATE, UpdateState},
};
use crate::proto::chat::v1 as w;

#[derive(Component)]
struct Approved;

#[derive(Component, Clone)]
struct PendingApproval {
    companion: String,
    request_id: String,
    agent: Entity,
}

pub(super) struct KernelPlugin;

impl Plugin for KernelPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(accepted)
            .add_observer(turn_input)
            .add_observer(release_approval)
            .add_systems(AgentPass, prepare.in_set(PrepareSystems::Edit))
            .add_systems(AgentPass, fit.in_set(PrepareSystems::Fit))
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            // Read before Commit removes the completed turn and its calls.
            .add_systems(AgentPass, streams.in_set(Stage::Review))
            .add_systems(PostUpdate, recalled)
            .add_systems(Last, changes.after(WriteLogs));
    }
}

fn release_approval(
    removed: On<Remove<PendingApproval>>,
    approvals: Query<&PendingApproval>,
    mut commands: Commands,
) {
    let Ok(pending) = approvals.get(removed.entity) else {
        return;
    };
    let pending = pending.clone();
    commands.queue(move |world: &mut World| {
        let result = (|| {
            let db = &world.resource::<Host>().db;
            if let Some(mut request) = db.question(&pending.companion, &pending.request_id)?
                && request.state == "pending"
            {
                request.state = "interrupted".into();
                db.put_question(&pending.companion, &request)?;
            }
            Ok::<_, ConnectError>(())
        })();
        if let Err(error) = result {
            fail(world, error.to_string());
        }
        conversation::changed(world, pending.agent);
    });
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
    let input = conversation::turn_input(&conversation, turn.start);
    if !input.is_empty() {
        conversation.add_aside(Aside::new(
            "host.turn_input",
            Author::rig(),
            serde_json::json!({"start":turn.start.get(),"input_id":input}),
        ));
    }
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
        // Keep acceptance evidence even when a failed turn recalls its user entry.
        conversation.add_aside(Aside::new(
            "host.accepted",
            Author::rig(),
            serde_json::json!(request.0),
        ));
    }
}

fn prepare(
    mut calls: Query<(&mut ModelRequest, &ForAgent), Added<Admitted>>,
    owners: Query<&Owner>,
    host: Res<Host>,
) {
    for (mut call, agent) in &mut calls {
        let Ok(owner) = owners.get(agent.0) else {
            continue;
        };
        let Ok(state) = host.db.state(owner.companion()) else {
            continue; // Gate below refuses if the canonical state cannot be read.
        };
        if let Some(prompt) = &mut call.prompt {
            prompt
                .sections
                .retain(|section| section.tag != "product_state");
            // Work gets its explicit brief, never the parent's private product state.
            if !owner.is_work() {
                prompt.sections.push(Section::new(
                    "product_state",
                    serde_json::to_string(&state).unwrap_or_default(),
                ));
            }
        }
        call.request_mut().additional_params = Some(serde_json::json!({"store": false}));
    }
}

/// Fits each model call to the window of the model it goes to.
///
/// `ModelRequest::history` is the request's own view of the conversation, so
/// cutting it changes neither the session log nor `GetHistory`. A model whose
/// window is unknown is sent the whole conversation, never a guess.
fn fit(mut calls: Query<(&mut ModelRequest, &Adapted), Added<Admitted>>) {
    for (mut call, adapted) in &mut calls {
        let spec = &adapted.0.spec;
        if let Some(max) = spec.max_output_tokens {
            call.request_mut().max_tokens = Some(u64::from(max));
        }
        let Some(window) = spec.context_window else {
            continue;
        };
        let counter = HeuristicTokenCounter::default();
        let reserve = spec.max_output_tokens.map_or(0, |tokens| tokens as usize)
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
        // A byte-ratio estimate, not the provider's tokenizer: it errs towards
        // a larger window, never a shorter one.
        let budget = (window as usize).saturating_sub(reserve);
        let Some(history) = call.history.as_mut() else {
            continue;
        };
        let kept = rig_memory::recent::view(history.messages(), &TokenBudget::new(budget));
        let len = history.messages().len();
        // The newest message is the question being asked: it always stays.
        let start = len.saturating_sub(kept.len()).min(len.saturating_sub(1));
        if start > 0 {
            history.splice(0..start, [], Author::plugin("host.window"));
        }
    }
}

fn decide(world: &mut World, entity: Entity, why: impl Into<String>) {
    if let Some(mut decision) = world.get_mut::<Decision>(entity) {
        decision.decide(Decider::plugin("host-permission"), Outcome::refused(why));
    }
}

fn gate(world: &mut World) {
    let calls: Vec<_> = world
        .query_filtered::<Entity, (ReadyToStart, Or<(With<ToolRequest>, With<ModelRequest>)>)>()
        .iter(world)
        .collect();
    for call in calls {
        if let Err(error) = guard(world, call) {
            if is_storage(&error) {
                fail(world, error.to_string());
            }
            decide(
                world,
                call,
                error.message.unwrap_or_else(|| "operation denied".into()),
            );
        }
    }
}

fn guard(world: &mut World, call: Entity) -> Result<()> {
    world.resource::<Host>().ready()?;
    let agent = world
        .get::<ForAgent>(call)
        .ok_or_else(|| ConnectError::permission_denied("unbound call"))?
        .0;
    let owner = world
        .get::<Owner>(agent)
        .ok_or_else(|| ConnectError::permission_denied("unbound agent"))?
        .clone();
    let config = conversation::settings(world, owner.companion())?;
    conversation::state(world, owner.companion())?;
    if world.get::<ModelRequest>(call).is_some() {
        let valid = world.get::<Binding>(agent).is_some_and(|binding| {
            binding.revision == config.revision && binding.permit.load(Ordering::Acquire)
        });
        if !config.model_allowed || !valid {
            return Err(ConnectError::permission_denied("model permission changed"));
        }
    }
    if let Some(request) = world.get::<ToolRequest>(call) {
        if !config.tools_allowed {
            return Err(ConnectError::permission_denied("tool permission revoked"));
        }
        if request.call.function.name.as_str() == GUARDED_UPDATE
            && !world.entity(call).contains::<Approved>()
        {
            let args = UpdateState::from_call(request)
                .ok_or_else(|| ConnectError::invalid_argument("invalid update"))?;
            let id = owner.conversation_id();
            let run = conversation::runs(world, agent, &id)
                .last()
                .map(|run| run.run_id.clone())
                .unwrap_or_default();
            let request_id = Uuid::new_v4().to_string();
            let pending = w::PendingRequest {
                request_id: request_id.clone(),
                conversation_id: id,
                run_id: run,
                state: "pending".into(),
                settings_revision: config.revision,
                value: args.value,
                expected_revision: args.expected_revision,
                ..Default::default()
            };
            world
                .resource::<Host>()
                .db
                .put_question(owner.companion(), &pending)?;
            world.spawn((
                HoldOf(call),
                PendingApproval {
                    companion: owner.companion().into(),
                    request_id,
                    agent,
                },
            ));
            conversation::changed(world, agent);
        }
    }
    Ok(())
}

pub(crate) fn respond(app: &mut App, request: w::RespondRequest) -> Result<w::Empty> {
    conversation::validate_id(&request.companion_id)?;
    conversation::validate_id(&request.request_id)?;
    let mut pending = app
        .world()
        .resource::<Host>()
        .db
        .question(&request.companion_id, &request.request_id)?
        .ok_or_else(|| ConnectError::not_found("request not found"))?;
    let resolved = if request.approve {
        "approved"
    } else {
        "denied"
    };
    if pending.state == resolved && pending.settings_revision == request.settings_revision {
        return Ok(w::Empty::default());
    }
    if pending.state != "pending" {
        return Err(ConnectError::failed_precondition(
            "request is no longer pending",
        ));
    }
    let config = conversation::settings(app.world(), &request.companion_id)?;
    if !config.tools_allowed
        || config.revision != pending.settings_revision
        || config.revision != request.settings_revision
    {
        return Err(ConnectError::aborted("permission or settings changed"));
    }
    let (hold, call) = app
        .world_mut()
        .query::<(Entity, &PendingApproval, &HoldOf)>()
        .iter(app.world())
        .find_map(|(hold, p, call)| {
            (p.companion == request.companion_id && p.request_id == request.request_id)
                .then_some((hold, call.0))
        })
        .ok_or_else(|| ConnectError::failed_precondition("run was interrupted"))?;
    let Some(operation) = app
        .world()
        .get::<ToolRequest>(call)
        .filter(|_| !app.world().entity(call).contains::<Started>())
    else {
        return Err(ConnectError::failed_precondition(
            "operation is no longer waiting",
        ));
    };
    let args = UpdateState::from_call(operation)
        .ok_or_else(|| ConnectError::aborted("operation arguments changed"))?;
    if args.value != pending.value || args.expected_revision != pending.expected_revision {
        return Err(ConnectError::aborted("operation arguments changed"));
    }
    if request.approve
        && conversation::state(app.world(), &request.companion_id)?.revision
            != pending.expected_revision
    {
        return Err(ConnectError::aborted("state revision changed"));
    }
    pending.state = resolved.into();
    app.world()
        .resource::<Host>()
        .db
        .put_question(&request.companion_id, &pending)?;
    if request.approve {
        app.world_mut().entity_mut(call).insert(Approved);
    } else {
        decide(app.world_mut(), call, "owner denied the operation");
    }
    app.world_mut().despawn(hold);
    if let Some(agent) = app.world().get::<ForAgent>(call).map(|agent| agent.0) {
        conversation::changed(app.world_mut(), agent);
    }
    Ok(w::Empty::default())
}

fn streams(
    mut deltas: MessageReader<StreamDelta>,
    agents: Query<(&Owner, &AgentId, &ActiveTurn)>,
    turns: Query<&Turn>,
    calls: Query<&Effect>,
    mut offsets: Local<HashMap<Entity, u64>>,
    mut host: ResMut<Host>,
) {
    offsets.retain(|entity, _| calls.contains(*entity));
    for delta in deltas.read() {
        let Some(agent) = delta.agent else {
            continue;
        };
        let Ok((owner, id, active)) = agents.get(agent) else {
            continue;
        };
        let Ok(turn) = turns.get(active.turn()) else {
            continue;
        };
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        let Ok(effect) = calls.get(delta.effect) else {
            continue;
        };
        let offset = offsets.entry(delta.effect).or_default();
        host.outbox.push(w::Event {
            companion_id: owner.companion().into(),
            conversation_id: id.0.clone(),
            run_id: owner.run_id(turn.start),
            kind: Some(w::event::Kind::TextDelta(Box::new(w::TextDelta {
                text: text.clone(),
                offset: *offset,
                call_id: effect.id().to_string(),
                ..Default::default()
            }))),
            ..Default::default()
        });
        *offset += text.chars().count() as u64;
    }
}

fn changes(
    mut changes: MessageReader<ConversationChanged>,
    attached: Query<Entity, Added<Owner>>,
    owners: Query<(Entity, &Owner)>,
    mut host: ResMut<Host>,
    log: Res<SessionLog>,
    mut commands: Commands,
) {
    if host.failed.is_some() || log.failure().is_some() {
        return;
    }
    // Added owners also reconcile restored work, which emits no conversation changes.
    let agents: HashSet<_> = changes
        .read()
        .map(|change| change.agent)
        .chain(attached.iter())
        .collect();
    let mut companions = HashSet::new();
    for agent in agents {
        if let Ok((_, owner)) = owners.get(agent) {
            companions.insert(owner.companion());
            host.notify_changed(&owner.0);
        }
    }
    // Reconcile both a worker's result and the parent's saved receipt of its report.
    if !host.closing {
        for (worker, owner) in &owners {
            if owner.is_work() && companions.contains(owner.companion()) {
                commands.queue(move |world: &mut World| {
                    if world.resource::<Host>().ready().is_ok()
                        && let Err(error) = report_work(world, worker)
                    {
                        fail(world, error.to_string());
                    }
                });
            }
        }
    }
}

fn recalled(
    mut recalled: MessageReader<Recalled>,
    agents: Query<(&Owner, &Conversation)>,
    mut host: ResMut<Host>,
) {
    for input in recalled.read() {
        let Ok((owner, conversation)) = agents.get(input.agent) else {
            continue;
        };
        let turn = conversation.turns().next_back().map(|(_, turn)| turn);
        host.outbox.push(w::Event {
            companion_id: owner.companion().into(),
            conversation_id: owner.conversation_id(),
            run_id: turn
                .as_ref()
                .map_or_else(String::new, |turn| owner.run_id(turn.start)),
            kind: Some(w::event::Kind::Recalled(Box::new(w::Recalled {
                input_id: turn.map_or_else(String::new, |turn| {
                    conversation::turn_input(conversation, turn.start)
                }),
                text: input.text.clone(),
                why: input.why.clone().unwrap_or_default(),
                ..Default::default()
            }))),
            ..Default::default()
        });
    }
}

fn report_work(world: &mut World, worker: Entity) -> Result<()> {
    let Some(owner) = world.get::<Owner>(worker) else {
        return Ok(());
    };
    let Some(mut work) = world.resource::<Host>().db.work(&owner.0.work_id)? else {
        return Ok(());
    };
    let Some(log) = world.get::<Conversation>(worker) else {
        return Ok(());
    };
    let Some((_, turn)) = log
        .turns()
        .next_back()
        .filter(|(_, turn)| turn.end.is_some())
    else {
        return Ok(());
    };
    let run_id = format!("{}:{}", work.info.conversation_id, turn.start.get());
    if run_id == work.reported {
        return Ok(());
    }
    let (state, failure) = conversation::turn_status(turn.end.as_ref());
    let text = log
        .entries()
        .rev()
        .take_while(|entry| entry.id >= turn.start)
        .find_map(|entry| match entry.message {
            message::Message::Assistant(message) => {
                let text = message.text();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .unwrap_or_default();
    let report = format!(
        "Work {} ended: {}. {}\n{}",
        work.info.work_id, state, failure, text
    );
    let parent = conversation::target_entity(
        world,
        &w::Target {
            companion_id: work.info.companion_id.clone(),
            ..Default::default()
        },
    )?;
    let report_id = format!("report:{run_id}");
    if conversation::has_request(world, parent, &report_id) {
        work.reported = run_id;
        world.resource::<Host>().db.put_work(&work)?;
    } else {
        // Inform stores the report without starting a new parent model call.
        let author = Author::agent(work.info.work_id, Some(RequestId(report_id)));
        world.trigger(Deliver::new(parent, report, DeliveryMode::Inform).with_author(author));
    }
    Ok(())
}
