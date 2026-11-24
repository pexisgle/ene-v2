//! The plugin hand: a work session whose backend is a harness guest instead
//! of the in-process rig agent.
//!
//! The backend pick happens at `start_run`: a companion whose task
//! assignment names an ACP connection runs on the harness plugin the
//! connection's `endpoint.adapter` installs; every other assignment keeps
//! the rig agent. The host serves no application services to a harness guest
//! yet, so a guest that calls one is refused at the service boundary.
//!
//! Concurrency is bounded and honest: the runtime serves one instance per
//! (adapter, extension) and the guest keeps its per-session state in
//! instance memory, so ONE plugin-hand `WorkSession` per adapter is live at a
//! time. A second `StartWork` on the same connection while a session holds
//! the adapter is refused with an error naming the busy harness; `CloseWork`
//! releases the claim. A stop mid-invocation kills the guest call, so the
//! next request reloads a fresh instance and the hand's session starts over
//! with `Start`; a stop while the turn is parked on a permission request
//! delivers `Cancel` and keeps the session for the next `Prompt`. A host
//! restart loses the guest state entirely, so a restore settles an open
//! plugin-hand run as failed instead of pretending the turn survived.

use super::{Run, RunState, WorkSession};
use crate::host::{
    background::{self, AbortOnDropHandle},
    db::{Result, Store},
    error::HostError,
    providers::{Providers, Running},
    resources::HostRuntime,
};
use bevy_ecs::prelude::*;
use ene_plugin::{Extension, HarnessInput, Operation, PermissionOption};
use ene_plugin_runtime::instance::{DenyServices, Services};
use ene_work::harness::{HarnessBuffer, prompt_of};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::mpsc;

type DriverOutcome = std::result::Result<Value, HostError>;

/// The one ECS operation the hand needs: despawning a held tool-call entity.
///
/// Isolating it keeps the hand's session bookkeeping independent of `World`;
/// the host supplies the real implementation, tests can supply a stub.
pub(crate) struct CallDespawner<'w> {
    world: &'w mut World,
}

impl<'w> CallDespawner<'w> {
    pub(crate) fn new(world: &'w mut World) -> Self {
        Self { world }
    }

    /// Despawns `call` when it is still alive. A stale entity is not an error:
    /// permission cleanup may already have removed it.
    pub(super) fn despawn(&mut self, call: Entity) {
        if self.world.get_entity(call).is_ok() {
            self.world.entity_mut(call).despawn();
        }
    }
}

/// The plugin hands this host is running, keyed by the adapter each holds.
/// One hand per adapter, for as long as the work session it serves is open.
#[derive(Resource, Default)]
pub(crate) struct Hands {
    pub(super) by_adapter: HashMap<String, Hand>,
}

pub(super) struct Hand {
    pub(super) work_id: String,
    pub(super) companion_id: String,
    /// The started harness instance. The runtime serves one per adapter, so
    /// holding it is the claim on the adapter.
    pub(super) running: Arc<Running>,
    /// The invocation in flight, if any. One at a time: a turn spans several
    /// invocations (`Start`, `Respond`, `Prompt`) but never overlaps them.
    pub(super) driver: Option<AbortOnDropHandle<DriverOutcome>>,
    /// The events channels that may still hold something to collect. A new
    /// invocation pushes its channel here instead of replacing one, because
    /// dropping a receiver a driver still writes to cancels that call and
    /// kills the guest; a drained, closed channel leaves on its own.
    pub(super) events: Vec<mpsc::Receiver<ene_plugin::Event>>,
    /// Message chunk accumulator and tool-name lookup across updates.
    pub(super) buffer: HarnessBuffer,
    /// Input accepted while a run was active; it becomes the next prompt.
    pub(super) parked: VecDeque<String>,
    /// The approval the turn is parked on, if any.
    pub(super) pending: Option<Pending>,
}

pub(super) struct Pending {
    /// The host approval row the request became.
    pub(super) approval: String,
    /// The held tool-call entity; despawning it lets the permission module's
    /// orphan cleanup release the hold and expire the row.
    pub(super) call: Entity,
    /// The request id the plugin is waiting on.
    pub(super) request: String,
    /// The options the plugin offered; the reply is drawn from them.
    pub(super) options: Vec<PermissionOption>,
}

/// The harness a companion's task assignment names, when it is an ACP
/// connection onto an installed harness plugin. Anything else keeps the
/// in-process rig agent.
pub(crate) fn selected(
    store: &Store,
    providers: Option<&Providers>,
    companion: &crate::host::mind::Companion,
) -> Option<(String, Arc<Running>)> {
    let assignment = companion.model(crate::host::mind::Role::Task)?;
    let settings = store.settings_repo().settings().ok()?;
    let connection = settings
        .connections
        .iter()
        .find(|connection| connection.id == assignment.connection_id)?;
    if connection.kind != crate::host::settings::ConnectionKind::Acp {
        return None;
    }
    let adapter = connection.endpoint().adapter.clone();
    let running = providers?.running(&adapter, Extension::Harness)?;
    Some((adapter, running))
}

/// The session, run and instruction an invocation is started for.
pub(crate) struct RunStart<'a> {
    pub(crate) session: &'a WorkSession,
    pub(crate) run: &'a Run,
    pub(crate) instruction: &'a str,
}

/// Starts a run on the plugin hand: claims the adapter for the session and
/// dispatches the first turn of the session as `Start`, or a follow-up as
/// `Prompt` on the session that already holds the adapter.
pub(crate) fn start_run(
    world: &mut World,
    hands: &mut Hands,
    start: RunStart<'_>,
    adapter: String,
    running: Arc<Running>,
) -> Result<()> {
    let RunStart {
        session,
        run,
        instruction,
    } = start;
    if let Some(owner) = hands.by_adapter.get(&adapter)
        && owner.work_id != session.id
    {
        return Err(HostError::Unavailable(format!(
            "the harness plugin `{adapter}` is already serving work session `{}`",
            owner.work_id
        )));
    }
    let claimed = hands.by_adapter.contains_key(&adapter);
    let input = if claimed {
        HarnessInput::Prompt {
            prompt: prompt_of(instruction),
        }
    } else {
        HarnessInput::Start {
            prompt: prompt_of(instruction),
            cwd: cwd(),
        }
    };
    hands.by_adapter.insert(
        adapter.clone(),
        Hand {
            work_id: session.id.clone(),
            companion_id: session.companion_id.clone(),
            running,
            driver: None,
            events: Vec::new(),
            buffer: HarnessBuffer::default(),
            parked: VecDeque::new(),
            pending: None,
        },
    );
    super::note_activity(&mut world.resource_mut(), &run.run_id);
    let runtime = world.resource::<HostRuntime>().0.clone();
    let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
    dispatch_to_hand(hands, &runtime, wake.as_ref(), &adapter, input);
    Ok(())
}

/// Whether a work session runs on the plugin hand.
pub(crate) fn owns(hands: &Hands, work_id: &str) -> bool {
    adapter_of(hands, work_id).is_some()
}

/// `send_work` while the hand's run is active: the guest is inside its turn
/// and cannot take a second prompt mid-invocation, so the input is parked
/// and becomes the next prompt turn once the run settles.
pub(crate) fn steer(hands: &mut Hands, work_id: &str, text: &str) {
    edit_hand(hands, work_id, |hand| {
        hand.parked.push_back(text.to_owned());
    });
}

/// Stops the hand's turn. A run mid-invocation is killed outright, which also
/// ends the session (the next request reloads a fresh guest); a run parked on
/// a permission request is told `Cancel` and keeps its session.
pub(crate) fn stop(
    hands: &mut Hands,
    calls: &mut CallDespawner<'_>,
    runtime: &tokio::runtime::Handle,
    wake: Option<&rig_ecs::tasks::Wake>,
    work_id: &str,
) {
    let Some(adapter) = adapter_of(hands, work_id) else {
        return;
    };
    let (running, in_flight, parked) = {
        let Some(hand) = hands.by_adapter.get(&adapter) else {
            return;
        };
        (
            Arc::clone(&hand.running),
            hand.driver
                .as_ref()
                .is_some_and(|driver| !driver.is_finished()),
            hand.pending.is_some(),
        )
    };
    if in_flight {
        edit_hand(hands, work_id, |hand| {
            // Dropping the driver aborts the call, which leaves the instance
            // without its guest. The next request reloads one whose session
            // state is empty, so the hand's claim on the adapter ends here.
            hand.driver = None;
            hand.buffer.clear_message();
            hand.parked.clear();
        });
        drop_pending(calls, hands, &adapter);
        running.note_cancelled();
        hands.by_adapter.remove(&adapter);
        return;
    }
    // The turn is parked on a permission request (or already drained): tell
    // the guest its turn was cancelled, and let the approval row expire.
    if parked {
        edit_hand(hands, work_id, |hand| {
            hand.buffer.clear_message();
            hand.parked.clear();
        });
        drop_pending(calls, hands, &adapter);
        dispatch_to_hand(hands, runtime, wake, &adapter, HarnessInput::Cancel);
    }
}

/// `CloseWork` drops the harness session: the hand releases its adapter claim
/// and the guest keeps whatever state it kept, as the concurrency bound above
/// documents. An invocation still in flight dies with the hand, so the next
/// request reloads a fresh guest.
pub(crate) fn release(calls: &mut CallDespawner<'_>, hands: &mut Hands, work_id: &str) {
    let Some(adapter) = adapter_of(hands, work_id) else {
        return;
    };
    // A parked approval, if any, is a hold on a call of the session being
    // closed; dropping the call lets the orphan path expire the row.
    drop_pending(calls, hands, &adapter);
    let in_flight = hands.by_adapter.get(&adapter).is_some_and(|hand| {
        hand.driver
            .as_ref()
            .is_some_and(|driver| !driver.is_finished())
    });
    if in_flight {
        // Closing the session drops the hand, which drops the events channel
        // an in-flight call is still writing to. That call then fails and
        // leaves the instance without its guest, so the next request must
        // reload one: abort the driver here and note the cancellation, the
        // same way a stop does. Leaving it to the drop would strand the
        // adapter on an unusable instance.
        edit_hand(hands, work_id, |hand| {
            hand.driver = None;
        });
    }
    if let Some(hand) = hands.by_adapter.remove(&adapter)
        && in_flight
    {
        hand.running.note_cancelled();
    }
}

/// Restores nothing but settles honestly: a restart lost the guest's session
/// state, so an open run this backend would serve reads failed naming the
/// restart. Runs the rig agent serves keep their own restore path.
pub(crate) fn settle_restarts(world: &mut World, run: Run) {
    super::finish_run(
        world,
        run,
        RunState::Failed,
        Some(
            "作業を続けられませんでした: ホストの再起動でハーネスのセッション状態が失われました"
                .to_owned(),
        ),
    );
}

/// Takes the pending approval out of the hand and drops its held call, which
/// the permission module's orphan path answers by releasing the hold and
/// expiring the row.
fn drop_pending(calls: &mut CallDespawner<'_>, hands: &mut Hands, adapter: &str) {
    let call = hands
        .by_adapter
        .get_mut(adapter)
        .and_then(|hand| hand.pending.take())
        .map(|pending| pending.call);
    if let Some(call) = call {
        calls.despawn(call);
    }
}

fn adapter_of(hands: &Hands, work_id: &str) -> Option<String> {
    hands
        .by_adapter
        .iter()
        .find(|(_, hand)| hand.work_id == work_id)
        .map(|(adapter, _)| adapter.clone())
}

/// Runs `edit` on the hand that serves `work_id`, if one exists.
fn edit_hand<R>(hands: &mut Hands, work_id: &str, edit: impl FnOnce(&mut Hand) -> R) -> Option<R> {
    hands
        .by_adapter
        .values_mut()
        .find(|hand| hand.work_id == work_id)
        .map(edit)
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

pub(super) fn dispatch_to_hand(
    hands: &mut Hands,
    runtime: &tokio::runtime::Handle,
    wake: Option<&rig_ecs::tasks::Wake>,
    adapter: &str,
    input: HarnessInput,
) {
    let Some(hand) = hands.by_adapter.get_mut(adapter) else {
        return;
    };
    let running = Arc::clone(&hand.running);
    let services: Arc<dyn Services> = Arc::new(DenyServices);
    let (sender, events) = mpsc::channel(16);
    let driver = background::spawn(runtime, wake, async move {
        running
            .invoke_streaming(Operation::Harness { input }, services, sender)
            .await
    });
    hand.driver = Some(driver);
    hand.events.push(events);
}
