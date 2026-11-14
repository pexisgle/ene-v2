//! Staged Promotion for autonomous heartbeat ticks
//! (docs/features/autonomy/schedule.md §4, docs/design/tech-stack.md §B).
//!
//! A tick must never leave a no-op turn in the companion's append-only session
//! log, so it runs on a temporary agent instead of in the main conversation.
//! The temporary agent clones the companion's conversation, system prompt and
//! model connection; the tick is a *side call*, not a turn, so the kernel
//! never writes the reply into any conversation. Only a reply that says
//! something is spliced into the companion's `Conversation`, where the normal
//! projection and session log take it from there.
//!
//! The temporary agent stays out of the session log by construction. In
//! rig-ecs `session_log.rs`, conversations are saved by a query that needs
//! `&AgentId`, and saved components by a query that needs `With<Agent>`. The
//! temporary agent has no `Agent` marker, and it gets its `AgentId` only after
//! the last save point that saw its conversation as added; its conversation is
//! never changed afterwards, so nothing is ever queued for it. It is despawned
//! (never retired) when the tick settles.
use super::{
    agent,
    compaction::{self, InactivityTracker, Weigher},
    db::{Store, now},
    resources::HostState,
};
use bevy_ecs::prelude::*;
use rig_core::completion::{AssistantContent, AssistantMessage, CompletionResponse, Message};
use rig_core::conversation::{Author, Entry};
use rig_ecs::prelude::*;
use std::time::{Duration, Instant};

/// Ticks need this much user idle time, like the compaction soft condition.
pub(crate) const IDLE: Duration = Duration::from_secs(60);
/// A tick that has not settled by then is discarded, and its running call
/// cancelled.
const TIMEOUT: Duration = Duration::from_secs(120);
/// The instruction the head gets for a heartbeat tick. It decides between
/// speaking and `noop`, exactly as for a schedule firing.
const TICK: &str =
    "定期Tick: ユーザーはしばらく無操作です。今話しかけるべきか、noop か判断してください";

/// The companion a staged tick belongs to, on the temporary agent. It is not a
/// `CompanionAgent`: everything that projects or notifies companions must skip
/// it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) parent: Entity,
}

/// The temporary agent's side call, the effect the tick's model call runs on.
#[derive(Component, Clone, Copy, Debug)]
struct StagedTick {
    temp: Entity,
}

/// The one staged tick in flight, if any.
#[derive(Resource, Default)]
struct StagedTicks {
    in_flight: Option<InFlight>,
    /// When the last tick started, so heartbeat ticks keep the idle cadence
    /// instead of running on every frame an idle user leaves.
    last_started: Option<Instant>,
}

struct InFlight {
    temp: Entity,
    effect: Entity,
    /// The tick instruction as the model got it, promoted with the reply.
    prompt: String,
    started: Instant,
}

/// A companion as the tick reads it: its conversation clone, prompt, model
/// and whether it is busy.
type Candidate<'a> = (
    Entity,
    &'a agent::CompanionAgent,
    &'a Conversation,
    &'a SystemPrompt,
    Option<&'a Connection>,
    Has<ActiveTurn>,
);

/// Spawns the temporary agent and its side call once the user is idle and the
/// companion has no turn running.
fn begin_tick(
    mut commands: Commands,
    mut ticks: ResMut<StagedTicks>,
    store: Option<Res<Store>>,
    tracker: Option<Res<InactivityTracker>>,
    host: Option<Res<HostState>>,
    agents: Query<Candidate<'static>, Without<Staged>>,
) {
    if ticks.in_flight.is_some() {
        return;
    }
    if ticks.last_started.is_some_and(|last| last.elapsed() < IDLE) {
        return;
    }
    if host.is_some_and(|host| host.failed.is_some() || host.closing) {
        return;
    }
    let Some(tracker) = tracker else {
        return;
    };
    if tracker.idle_duration() < IDLE {
        return;
    }
    let Some(store) = store else {
        return;
    };
    // The first companion (in creation order) that can run a turn right now.
    for companion in store.companions().unwrap_or_default() {
        let Some((parent, _, conversation, prompt, connection, busy)) = agents
            .iter()
            .find(|(_, agent, ..)| agent.companion_id == companion.id)
        else {
            continue;
        };
        if busy {
            continue;
        }
        let Some(connection) = connection else {
            continue;
        };
        // The clone must not carry the companion's unsaved edits: a discarded
        // temporary agent may never queue them for a log.
        let mut history = conversation.0.clone();
        let _ = history.take_unsaved();
        let temp = commands
            .spawn((
                Name::new("staged-tick"),
                Staged { parent },
                Conversation(history),
                SystemPrompt(prompt.0.clone()),
                ToolAccess::Only(vec!["noop".to_owned()]),
                ReasoningSetting::default(),
                connection.to_owned(),
            ))
            .id();
        let effect = commands.spawn((StagedTick { temp }, SideCall(temp))).id();
        ticks.last_started = Some(Instant::now());
        ticks.in_flight = Some(InFlight {
            temp,
            effect,
            prompt: format!("{TICK}\n現在日時: {}", now()),
            started: Instant::now(),
        });
        return;
    }
}

/// Gives the temporary agent its `AgentId`, one frame after it was spawned:
/// the side call needs one to build its request, but the save path must never
/// see the agent with it while its conversation still counts as added.
fn arm_tick(world: &mut World) {
    let Some(temp) = world
        .get_resource::<StagedTicks>()
        .and_then(|ticks| ticks.in_flight.as_ref().map(|in_flight| in_flight.temp))
    else {
        return;
    };
    if world.get::<AgentId>(temp).is_some() || world.get_entity(temp).is_err() {
        return;
    }
    world
        .entity_mut(temp)
        .insert(AgentId(format!("staged:{}", uuid::Uuid::new_v4())));
}

/// Puts the tick instruction at the end of the request the kernel built from
/// the temporary agent: a user message the model answers, never saved.
fn request_tick(
    ticks: Res<StagedTicks>,
    mut calls: Query<&mut ModelRequest, (Added<Admitted>, With<StagedTick>)>,
) {
    let Some(in_flight) = ticks.in_flight.as_ref() else {
        return;
    };
    for mut call in &mut calls {
        call.history
            .get_or_insert_default()
            .push(Message::user(in_flight.prompt.clone()));
    }
}

/// A staged request as fitting reads it.
type Fitting<'a> = (&'a mut ModelRequest, &'a Adapted, &'a mut Decision);

/// Keeps the tick request within the model's window, like an ordinary turn.
fn fit_tick(mut calls: Query<Fitting<'static>, (Added<Admitted>, With<StagedTick>)>) {
    for (mut call, adapted, mut decision) in &mut calls {
        let limit = adapted
            .0
            .spec
            .input_limit()
            .map(u64::from)
            .unwrap_or(128_000);
        let reserve = u64::from(adapted.0.spec.max_output_tokens.unwrap_or(4_096)) + limit / 10;
        let ModelRequest {
            request,
            history,
            prompt,
            ..
        } = &mut *call;
        let prompt = prompt
            .as_ref()
            .map(rig_core::completion::SystemPrompt::text);
        let model = adapted.0.spec.reference();
        let weight = Weigher {
            model: &model,
            prompt: prompt.as_deref(),
            tools: &request.tools,
            anchor: None,
            sent: None,
        };
        let Some(view) = history.as_mut() else {
            continue;
        };
        if !compaction::fit(view, &weight, limit, reserve) {
            decision.decide(
                Decider::plugin("ene-host"),
                Outcome::refused("context_exceeded: the tick does not fit the model window"),
            );
        }
    }
}

/// The reply's text blocks, when the tick acted: it wrote something and did
/// not call `noop`.
fn acting_reply(response: &CompletionResponse) -> Option<AssistantMessage> {
    let noop = response.choice.iter().any(|content| {
        matches!(content, AssistantContent::ToolCall(call) if call.function.name == "noop")
    });
    if noop {
        return None;
    }
    let content: Vec<AssistantContent> = response
        .choice
        .iter()
        .filter(|content| matches!(content, AssistantContent::Text(_)))
        .cloned()
        .collect();
    let text: String = content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    (!text.trim().is_empty()).then(|| AssistantMessage::new(content))
}

/// Promotes an acting tick into the companion's conversation and discards a
/// silent one. The temporary agent never writes to the session log, so the
/// promotion is one splice the normal projection sees.
fn promote_tick(
    mut commands: Commands,
    settled: Query<(Entity, &StagedTick, &Outcome), Added<Settled>>,
    parents: Query<&Staged>,
    mut ticks: ResMut<StagedTicks>,
    mut conversations: Query<&mut Conversation>,
) {
    for (effect, tick, outcome) in &settled {
        if ticks
            .in_flight
            .as_ref()
            .is_none_or(|in_flight| in_flight.effect != effect)
        {
            continue;
        }
        let Some(in_flight) = ticks.in_flight.take() else {
            continue;
        };
        if let Ok(staged) = parents.get(tick.temp)
            && let Some(reply) = outcome.completion().and_then(acting_reply)
            && let Ok(mut conversation) = conversations.get_mut(staged.parent)
        {
            let at = conversation.messages().len();
            let by = Author::plugin("ene-host");
            conversation.splice(
                at..,
                [
                    Entry::new(Message::user(in_flight.prompt)).by(by.clone()),
                    Entry::new(Message::Assistant(reply)),
                ],
                by,
            );
        }
        // Not `Retire`: a retire record would be written for the temporary
        // agent. Without its id the discard hook skips it entirely.
        commands.entity(tick.temp).remove::<AgentId>();
        commands.entity(tick.temp).despawn();
    }
}

/// Drops a tick that never settled, cancelling its model call.
fn expire_tick(world: &mut World) {
    let expired = {
        let Some(ticks) = world.get_resource::<StagedTicks>() else {
            return;
        };
        ticks
            .in_flight
            .as_ref()
            .filter(|in_flight| in_flight.started.elapsed() > TIMEOUT)
            .map(|in_flight| (in_flight.temp, in_flight.effect))
    };
    let Some((temp, effect)) = expired else {
        return;
    };
    world.resource_mut::<StagedTicks>().in_flight = None;
    if let Ok(mut entity) = world.get_entity_mut(temp) {
        entity.remove::<AgentId>();
    }
    world.try_despawn(temp).ok();
    world.try_despawn(effect).ok();
}

/// Registers the staged heartbeat tick.
pub(crate) struct StagedPlugin;

impl Plugin for StagedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StagedTicks>()
            .add_systems(AgentPass, request_tick.in_set(PrepareSystems::Edit))
            .add_systems(AgentPass, fit_tick.in_set(PrepareSystems::Fit))
            .add_systems(Update, arm_tick.before(AgentLoop))
            .add_systems(
                Update,
                (begin_tick, promote_tick, expire_tick)
                    .chain()
                    .after(AgentLoop),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        autonomy::testing::{Harness, text_turn},
        compaction::InactivityTracker,
        notify::Notifier,
    };
    use rig_core::test_utils::MockStreamEvent;
    use serde_json::json;

    /// The parent's conversation and the DirStore as they are, for the
    /// no-change half of the tests.
    fn snapshot(harness: &mut Harness, agent: Entity) -> (usize, Vec<String>, Option<Vec<u8>>) {
        let len = harness.conversation(agent).messages().len();
        let names = harness.log_names();
        let bytes = names.first().and_then(|name| {
            std::fs::read(harness.root.path().join("rig").join("logs").join(name)).ok()
        });
        (len, names, bytes)
    }

    fn idle(harness: &mut Harness) {
        harness
            .app
            .world_mut()
            .resource_mut::<InactivityTracker>()
            .last_user_interaction_at = Instant::now() - IDLE - Duration::from_secs(1);
    }

    /// Runs one tick to its end: it is spawned, then settles or expires.
    fn run_tick(harness: &mut Harness) {
        harness.run_until(|world| {
            let mut query = world.query_filtered::<Entity, With<Staged>>();
            query.iter(world).next().is_some()
        });
        harness.run_until(|world| world.resource::<StagedTicks>().in_flight.is_none());
    }

    fn answer(harness: &mut Harness) -> Vec<u8> {
        let agent = harness.companion_agent();
        harness.run_until(move |world| {
            world
                .get::<Conversation>(agent)
                .is_some_and(|conversation| {
                    conversation
                        .entries()
                        .any(|entry| entry_text_is_assistant(&entry))
                })
        });
        harness.tick();
        harness
            .log_names()
            .first()
            .and_then(|name| {
                std::fs::read(harness.root.path().join("rig").join("logs").join(name)).ok()
            })
            .unwrap_or_default()
    }

    fn entry_text_is_assistant(entry: &rig_core::conversation::EntryRef<'_>) -> bool {
        matches!(entry.message, Message::Assistant(_))
    }

    #[test]
    fn a_silent_tick_leaves_the_conversation_and_the_log_unchanged() {
        let mut harness = Harness::new([
            text_turn("こんにちは"),
            vec![
                MockStreamEvent::tool_call("call-1", "noop", json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = harness.companion_agent();
        // One ordinary turn first, so the temporary agent's clone holds
        // entries that a leaking projection would publish.
        crate::host::chat::accept(
            &mut harness.app,
            ene_rpc::SendMessageRequest {
                companion_id: harness.companion.clone(),
                request_key: "key-1".into(),
                text: "ただいま".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let _ = answer(&mut harness);
        let (len, names, bytes) = snapshot(&mut harness, agent);
        assert_eq!(names.len(), 1, "the companion's own log only");

        idle(&mut harness);
        let mut notifications = harness.app.world().resource::<Notifier>().subscribe();
        run_tick(&mut harness);

        // Nothing reached the companion, and the append-only log did not grow.
        assert_eq!(harness.conversation(agent).messages().len(), len);
        assert_eq!(harness.log_names(), names);
        let after = snapshot(&mut harness, agent).2;
        assert_eq!(after, bytes, "no no-op tick is written to the DirStore");
        // The next tick waits for the next idle interval.
        harness.tick();
        let mut query = harness
            .app
            .world_mut()
            .query_filtered::<Entity, With<Staged>>();
        assert!(
            query.iter(harness.app.world()).next().is_none(),
            "a silent tick does not start another tick in the same idle window"
        );
        // No notification leaked from the temporary agent (an empty companion).
        while let Ok(notification) = notifications.try_recv() {
            assert!(
                notification
                    .companion_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty()),
                "a staged tick must never publish a message with an empty companion"
            );
        }
    }

    #[test]
    fn an_acting_tick_promotes_exactly_its_input_and_reply() {
        let mut harness = Harness::new([
            text_turn("こんにちは"),
            text_turn("おはよう。そろそろ休憩しない？"),
        ]);
        let agent = harness.companion_agent();
        crate::host::chat::accept(
            &mut harness.app,
            ene_rpc::SendMessageRequest {
                companion_id: harness.companion.clone(),
                request_key: "key-1".into(),
                text: "ただいま".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let _ = answer(&mut harness);
        let (len, _, _) = snapshot(&mut harness, agent);

        idle(&mut harness);
        let mut notifications = harness.app.world().resource::<Notifier>().subscribe();
        run_tick(&mut harness);
        harness.tick();

        let conversation = harness.conversation(agent);
        assert_eq!(conversation.messages().len(), len + 2);
        let entries: Vec<_> = conversation.entries().skip(len).collect();
        assert_eq!(entries.len(), 2);
        let input = agent::entry_text(&entries[0]).expect("the promoted input text");
        assert!(input.starts_with("定期Tick: "), "{input}");
        assert_eq!(
            entries[0].author.map(|author| &author.kind),
            Some(&rig_core::conversation::AuthorKind::Plugin(
                "ene-host".into()
            ))
        );
        let reply = agent::entry_text(&entries[1]).expect("the promoted reply text");
        assert_eq!(reply, "おはよう。そろそろ休憩しない？");
        assert!(entries[1].author.is_none(), "the reply is the assistant's");

        // The promotion is written once, under the companion's own log: no
        // `staged:*` log appears.
        let names = harness.log_names();
        assert_eq!(
            names,
            vec![format!("companion:{}.jsonl", harness.companion)],
            "the temporary agent has no session log"
        );
        while let Ok(notification) = notifications.try_recv() {
            assert!(
                notification
                    .companion_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty()),
                "a staged tick must never publish a message with an empty companion"
            );
        }
    }

    #[test]
    fn a_tick_waits_for_the_companion_turn_to_end() {
        let mut harness = Harness::new([text_turn("こんにちは")]);
        harness
            .app
            .add_systems(AgentPass, hold_turn_calls.in_set(Stage::Gate));
        let agent = harness.companion_agent();
        crate::host::chat::accept(
            &mut harness.app,
            ene_rpc::SendMessageRequest {
                companion_id: harness.companion.clone(),
                request_key: "key-1".into(),
                text: "ただいま".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            harness.app.world().get::<ActiveTurn>(agent).is_some(),
            "the held turn stays active"
        );
        idle(&mut harness);
        harness.tick();
        let mut query = harness
            .app
            .world_mut()
            .query_filtered::<Entity, With<Staged>>();
        assert!(
            query.iter(harness.app.world()).next().is_none(),
            "the tick must not run beside an active turn"
        );

        // Release the turn; once it ends, the next frame starts a tick.
        let holds: Vec<Entity> = {
            let mut query = harness
                .app
                .world_mut()
                .query_filtered::<Entity, With<HoldOf>>();
            query.iter(harness.app.world()).collect()
        };
        for hold in holds {
            harness.app.world_mut().despawn(hold);
        }
        harness.run_until(|world| world.get::<ActiveTurn>(agent).is_none());
        harness.tick();
        assert!(
            harness
                .app
                .world()
                .resource::<StagedTicks>()
                .last_started
                .is_some(),
            "the tick runs once the companion is idle again"
        );
    }

    /// Holds every admitted turn call, so a test can keep a turn active.
    fn hold_turn_calls(
        mut commands: Commands,
        calls: Query<Entity, (With<TurnCall>, Added<Admitted>)>,
    ) {
        for call in &calls {
            commands.spawn(HoldOf(call));
        }
    }
}
