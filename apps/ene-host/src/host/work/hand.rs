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

use super::{Run, WorkSession};
use crate::host::{
    background::AbortOnDropHandle,
    db::{Result, Store},
    error::HostError,
    providers::{Providers, Running},
};
use bevy_ecs::prelude::*;
use ene_plugin::{Extension, HarnessEvent, HarnessInput, Operation, PermissionOption};
use ene_plugin_runtime::instance::DenyServices;
use ene_work::harness::HarnessBuffer;
use rig_ecs::tasks::Wake;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, oneshot};

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
    pub(super) turn: Turn,
    pub(super) invocation: Option<Invocation>,
    /// Message chunk accumulator and tool-name lookup across updates.
    pub(super) buffer: HarnessBuffer,
    /// The approval the turn is parked on, if any.
    pub(super) pending: Option<Pending>,
}

/// Only Run accepts guest events. Cancel and Ready never attribute the
/// previous invocation's results to another run.
pub(super) enum Turn {
    New,
    Run(String),
    Cancel,
    Ready,
}

pub(super) struct Invocation {
    pub(super) events: mpsc::Receiver<ene_plugin::Event>,
    pub(super) outcome: oneshot::Receiver<Result<()>>,
    pub(super) terminal: Option<HarnessEvent>,
    _task: AbortOnDropHandle<()>,
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

pub(super) struct SelectedHarness {
    pub(super) connection_id: ene_domain::ConnectionId,
    pub(super) adapter: String,
    pub(super) running: Arc<Running>,
}

/// Selects the installed harness for an explicit ACP task assignment.
pub(super) fn selected(
    store: &Store,
    providers: Option<&Providers>,
    companion: &crate::host::mind::Companion,
) -> Result<Option<SelectedHarness>> {
    let Some(assignment) = companion.model(crate::host::mind::Role::Task) else {
        return Ok(None);
    };
    let Some(connection) = store
        .settings_repo()
        .connection(&assignment.connection_id)?
    else {
        return Ok(None);
    };
    if connection.kind != crate::host::settings::ConnectionKind::Acp {
        return Ok(None);
    }
    if !connection.enabled {
        return Err(HostError::FailedPrecondition(
            "the assigned harness connection is disabled".into(),
        ));
    }
    let adapter = connection.endpoint().adapter.clone();
    let running = providers
        .and_then(|providers| providers.running(&adapter, Extension::Harness))
        .ok_or_else(|| HostError::Unavailable(format!("harness `{adapter}` is unavailable")))?;
    Ok(Some(SelectedHarness {
        connection_id: connection.id,
        adapter,
        running,
    }))
}

/// Claims the selected instance. The collector dispatches the persisted
/// instruction only after any invocation left by the previous run is drained.
pub(crate) fn claim(
    hands: &mut Hands,
    session: &WorkSession,
    harness: Option<SelectedHarness>,
) -> Result<Option<Entity>> {
    let Some(SelectedHarness {
        adapter, running, ..
    }) = harness
    else {
        return Ok(release(hands, &session.id));
    };
    if let Some(owner) = hands.by_adapter.get(&adapter) {
        if owner.work_id != session.id {
            return Err(HostError::Unavailable(format!(
                "the harness plugin `{adapter}` is already serving work session `{}`",
                owner.work_id
            )));
        }
        if Arc::ptr_eq(&owner.running, &running) {
            return Ok(None);
        }
    }
    let released = release(hands, &session.id);
    hands.by_adapter.insert(
        adapter,
        Hand {
            work_id: session.id.clone(),
            companion_id: session.companion_id.as_str().to_owned(),
            running,
            turn: Turn::New,
            invocation: None,
            buffer: HarnessBuffer::default(),
            pending: None,
        },
    );
    Ok(released)
}

/// Whether a work session runs on the plugin hand.
pub(crate) fn owns(hands: &Hands, work_id: &str) -> bool {
    adapter_of(hands, work_id).is_some()
}

/// Stops the exact persisted run, including one that was accepted while the
/// guest was still cancelling its predecessor.
pub(crate) fn stop(hands: &mut Hands, run: &Run) -> Option<Entity> {
    let adapter = adapter_of(hands, &run.work_id)?;
    let hand = hands.by_adapter.get_mut(&adapter)?;
    if !matches!(&hand.turn, Turn::Run(id) if *id == run.run_id) {
        return None;
    }
    if let Some(pending) = hand.pending.take() {
        hand.buffer = HarnessBuffer::default();
        hand.turn = Turn::Cancel;
        Some(pending.call)
    } else {
        release(hands, &run.work_id)
    }
}

pub(crate) fn release(hands: &mut Hands, work_id: &str) -> Option<Entity> {
    let adapter = adapter_of(hands, work_id)?;
    let hand = hands.by_adapter.remove(&adapter)?;
    drop(hand.invocation);
    hand.running.discard_harness_session();
    hand.pending.map(|pending| pending.call)
}

fn adapter_of(hands: &Hands, work_id: &str) -> Option<String> {
    hands
        .by_adapter
        .iter()
        .find(|(_, hand)| hand.work_id == work_id)
        .map(|(adapter, _)| adapter.clone())
}

pub(super) fn dispatch(
    hand: &mut Hand,
    runtime: &tokio::runtime::Handle,
    wake: Option<Wake>,
    input: HarnessInput,
) {
    debug_assert!(hand.invocation.is_none());
    let running = Arc::clone(&hand.running);
    let (sender, mut incoming) = mpsc::channel(16);
    let (forward, events) = mpsc::channel(16);
    let (result, outcome) = oneshot::channel();
    let task = runtime.spawn(async move {
        let relay = async {
            while let Some(event) = incoming.recv().await {
                if forward.send(event).await.is_err() {
                    break;
                }
                if let Some(wake) = &wake {
                    wake.wake();
                }
            }
        };
        let call =
            running.invoke_streaming(Operation::Harness { input }, Arc::new(DenyServices), sender);
        let (outcome, ()) = tokio::join!(call, relay);
        drop(forward);
        let _sent = result.send(outcome.map(|_| ()));
        if let Some(wake) = &wake {
            wake.wake();
        }
    });
    hand.invocation = Some(Invocation {
        events,
        outcome,
        terminal: None,
        _task: AbortOnDropHandle::new(task),
    });
}
