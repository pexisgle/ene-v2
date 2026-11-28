//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` survives runs and owns one backend: the long-lived
//! in-process rig agent, or the plugin hand (`hand`) a companion's ACP task
//! assignment picks. A `Run` is one instruction turn that rebinds the rig
//! agent to its run id. Only a closed session, a failed start, or a restart
//! retires the agent, never an ordinary run end.
mod hand;
mod hand_collect;
#[cfg(test)]
mod hand_tests;
mod input;
mod lifecycle;
mod notify;
mod reads;
mod settle;
mod skills;
mod tools;

use super::{
    agent,
    db::{Result, Store},
    error::{Failure, HostError},
    mind::Companion,
    notify::Notifier,
    providers::Providers,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

#[cfg(test)]
use crate::host::agent::{WorkAgent, WorkReports};
#[cfg(test)]
use rig_core::completion::Message;
#[cfg(test)]
use std::time::Instant;

use ene_domain::RunExecutor;
pub(crate) use ene_domain::{Run, RunState, WorkSession, WorkStatus};
pub(crate) use ene_work::{STALL_TIMEOUT, new_run};
pub(crate) use input::send;
pub(crate) use lifecycle::restore;
use lifecycle::{agent_of, ensure, maintain, retire};
use notify::{inform, notify_finished, notify_started, notify_status};
pub(crate) use reads::{get, list};
pub(crate) use settle::settle_run;
use settle::{finish_run, note_activity};

#[derive(Resource, Default)]
pub(super) struct Runs(ene_work::RunTracker);

impl std::ops::Deref for Runs {
    type Target = ene_work::RunTracker;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Runs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Resource, Default)]
struct ReadyWork(std::collections::BTreeSet<String>);

pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        skills::register(app);
        app.init_resource::<Runs>()
            .init_resource::<hand::Hands>()
            .init_resource::<ReadyWork>()
            .add_systems(
                Update,
                (hand_collect::collect, maintain).chain().after(AgentLoop),
            );
    }
}

fn require_companion(store: &Store, id: &str) -> Result<Companion> {
    store
        .companion_repo()
        .companion(id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))
}

fn require_session(store: &Store, id: &str) -> Result<WorkSession> {
    store
        .work_repo()
        .work_session(id)?
        .ok_or_else(|| HostError::NotFound(("work session not found").into()))
}

pub(crate) fn start(world: &mut World, request: &w::StartWorkRequest) -> Result<w::WorkSession> {
    let store = world.resource::<Store>().clone();
    let companion = require_companion(&store, &request.companion_id)?;
    if request.instruction.trim().is_empty() {
        return Err(HostError::Invalid(("an instruction is required").into()));
    }
    let client_id = binding_for(&store, request)?;
    let session = WorkSession {
        id: uuid::Uuid::new_v4().to_string(),
        companion_id: ene_domain::CompanionId::parse(companion.id.clone())?,
        title: if request.title.trim().is_empty() {
            ene_store::util::truncate(&request.instruction, 80)
        } else {
            request.title.clone()
        },
        status: WorkStatus::Idle,
        client_id,
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.work_repo().insert_work_session(&session)?;
    if let Err(error) = start_run(world, &session, &request.instruction) {
        store.work_repo().close_work(&session.id)?;
        retire(world, &session.id);
        return Err(error);
    }
    let stored = require_session(&store, &session.id)?;
    Ok(w::WorkSession::from(&stored))
}

struct Executor {
    companion: Companion,
    harness: Option<hand::SelectedHarness>,
}

impl Executor {
    fn recorded(&self) -> RunExecutor {
        self.harness
            .as_ref()
            .map_or(RunExecutor::Rig, |harness| RunExecutor::Harness {
                connection_id: harness.connection_id.clone(),
                plugin_id: harness.adapter.clone(),
                plugin_revision: harness.running.package_revision().to_owned(),
            })
    }
}

fn executor_of(
    store: &Store,
    session: &WorkSession,
    providers: Option<&Providers>,
) -> Result<Executor> {
    let companion = require_companion(store, session.companion_id.as_str())?;
    let harness = hand::selected(store, providers, &companion)?;
    Ok(Executor { companion, harness })
}

pub(crate) fn start_run(
    world: &mut World,
    session: &WorkSession,
    instruction: &str,
) -> Result<Run> {
    let store = world.resource::<Store>().clone();
    if session.status == WorkStatus::Closed {
        return Err(HostError::Conflict("work session is closed".into()));
    }
    if instruction.trim().is_empty() {
        return Err(HostError::Invalid("an instruction is required".into()));
    }
    if require_session(&store, &session.id)?.status == WorkStatus::Active {
        return Err(HostError::Conflict(
            "the work session already has work to run".into(),
        ));
    }
    let executor = executor_of(&store, session, world.get_resource::<Providers>())?;
    let run = new_run(
        uuid::Uuid::new_v4().to_string(),
        &session.id,
        instruction,
        RunState::Running,
    );
    store
        .work_repo()
        .insert_run(&run, &executor.recorded(), None)?;
    if let Err(error) = dispatch_run(world, session, &run, executor) {
        let failed = fail_run(&store, &run, &error)?;
        let session = require_session(&store, &session.id)?;
        let notifier = world.resource::<Notifier>();
        notify_status(notifier, &session);
        notify_finished(notifier, &session, &failed);
        return Err(error);
    }
    Ok(run)
}

fn dispatch_run(
    world: &mut World,
    session: &WorkSession,
    run: &Run,
    executor: Executor,
) -> Result<()> {
    let Executor { companion, harness } = executor;
    let is_harness = harness.is_some();
    let claimed = if is_harness
        || world
            .get_resource::<hand::Hands>()
            .is_some_and(|hands| hand::owns(hands, &session.id))
    {
        hand::claim(&mut world.resource_mut(), session, harness)
    } else {
        Ok(None)
    };
    if let Some(released) = claimed? {
        let _removed = world.try_despawn(released);
    }
    if is_harness {
        retire(world, &session.id);
    } else {
        let agent = ensure(world, &companion, &session.id, &run.run_id);
        agent::ensure_model(world, agent)?;
        let author = Author {
            request: Some(RequestId(run.run_id.clone())),
            ..Author::user()
        };
        world.trigger(
            Deliver::new(agent, run.instruction.clone(), DeliveryMode::Queue).with_author(author),
        );
    }
    if let Some(wake) = world.get_resource::<rig_ecs::tasks::Wake>() {
        wake.wake();
    }
    world.resource_mut::<Runs>().begin(&run.run_id);
    let session = require_session(world.resource::<Store>(), &session.id)?;
    let notifier = world.resource::<Notifier>();
    notify_status(notifier, &session);
    notify_started(notifier, &session, run);
    Ok(())
}

fn fail_run(store: &Store, run: &Run, error: &HostError) -> Result<Run> {
    let mut failed = run.clone();
    failed.state = RunState::Failed;
    failed.failure = Some(Failure::new(error.code(), error.to_string()));
    store.work_repo().update_run(&failed)?;
    Ok(failed)
}

/// The client a new work session is commissioned from: the one the request
/// names, or the companion's active client when it names none. The session
/// keeps that client even after the companion moves to another one.
fn binding_for(store: &Store, request: &w::StartWorkRequest) -> Result<Option<String>> {
    let activation = store.client_repo().activation(&request.companion_id)?;
    if request.client_id.is_empty() {
        return Ok(activation.and_then(|activation| activation.client_id));
    }
    ene_domain::validate_id(&request.client_id)?;
    store
        .client_repo()
        .client(&request.client_id)?
        .ok_or_else(|| HostError::NotFound("client not found".into()))?;
    let Some(activation) = activation else {
        return Err(HostError::Conflict(
            ("the companion has no active client to bind the session to").into(),
        ));
    };
    if activation.client_id.as_deref() != Some(request.client_id.as_str()) {
        return Err(HostError::Conflict(
            ("the requested client is not the active client of this companion").into(),
        ));
    }
    Ok(activation.client_id)
}

/// Stops the open run of a session. The stop is final at once: the work agent
/// is told to stop its turn, the run reads cancelled, and the parent is told.
/// Nothing waits for the agent's turn to unwind.
pub(crate) fn stop(world: &mut World, work_id: &str, reason: Option<&str>) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if require_session(&store, work_id)?.status == WorkStatus::Closed {
        return Ok(());
    }
    let waiting = store.work_repo().cancel_starting(work_id)?;
    if !waiting.is_empty() {
        let session = require_session(&store, work_id)?;
        let notifier = world.resource::<Notifier>();
        notify_status(notifier, &session);
        for run in waiting {
            notify_finished(notifier, &session, &run);
        }
    }
    let Some(run) = store.work_repo().active_run(work_id)? else {
        return Ok(());
    };
    if hand::owns(world.resource::<hand::Hands>(), work_id) {
        if let Some(call) = hand::stop(&mut world.resource_mut(), &run) {
            let _removed = world.try_despawn(call);
        }
    } else if let Some(agent) = agent_of(world, work_id) {
        let stop = reason.map_or_else(
            || StopTurn::new(agent),
            |reason| StopTurn::plugin(agent, "ene-host", reason),
        );
        world.trigger(stop);
    }
    finish_run(world, run, RunState::Cancelled, None);
    Ok(())
}

pub(crate) fn close(world: &mut World, work_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if require_session(&store, work_id)?.status == WorkStatus::Closed {
        return Ok(());
    }
    stop(world, work_id, Some("the work session is closing"))?;
    store.work_repo().close_work(work_id)?;
    if let Ok(Some(session)) = store.work_repo().work_session(work_id) {
        notify_status(world.resource::<Notifier>(), &session);
    }
    if let Some(call) = hand::release(&mut world.resource_mut(), work_id) {
        let _removed = world.try_despawn(call);
    }
    retire(world, work_id);
    Ok(())
}

#[cfg(test)]
mod tests;
