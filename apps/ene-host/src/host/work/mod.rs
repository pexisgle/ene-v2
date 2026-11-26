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
mod lifecycle;
mod notify;
mod reads;
mod settle;
mod skills;
mod tools;

use super::{
    agent::{self, WorkAgent},
    db::{Result, Store},
    error::{Failure, HostError},
    mind::Companion,
    notify::Notifier,
    providers::Providers,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use ene_store::util::now;
use rig_ecs::prelude::*;
use serde_json::json;

#[cfg(test)]
use crate::host::agent::WorkReports;
#[cfg(test)]
use rig_core::completion::Message;
#[cfg(test)]
use std::time::Instant;

pub(crate) use ene_domain::{Run, RunState, WorkSession, WorkStatus};
pub(crate) use ene_work::{STALL_TIMEOUT, new_run};
use lifecycle::{agent_of, bind_work_agent, detect_stalls, ensure, restore, retire};
use notify::{inform, notify_finished, notify_run, notify_status};
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

/// `SendWork` receipts used while the session agent is not in the world; its
/// log holds the canonical copy.
#[derive(Resource, Default)]
struct Receipts(ene_work::WorkReceipts);

impl std::ops::Deref for Receipts {
    type Target = ene_work::WorkReceipts;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Receipts {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        skills::register(app);
        app.init_resource::<Runs>()
            .init_resource::<Receipts>()
            .init_resource::<hand::Hands>()
            .add_systems(Startup, restore)
            .add_systems(
                Update,
                (hand_collect::collect, detect_stalls)
                    .chain()
                    .after(AgentLoop),
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
        let _written = store
            .work_repo()
            .set_work_status(&session.id, WorkStatus::Closed);
        retire(world, &session.id);
        return Err(error);
    }
    let stored = require_session(&store, &session.id)?;
    Ok(w::WorkSession::from(&stored))
}

pub(crate) fn start_run(
    world: &mut World,
    session: &WorkSession,
    instruction: &str,
) -> Result<Run> {
    let store = world.resource::<Store>().clone();
    if session.status == WorkStatus::Closed {
        return Err(HostError::Conflict(("work session is closed").into()));
    }
    if instruction.trim().is_empty() {
        return Err(HostError::Invalid(("an instruction is required").into()));
    }
    if let Some(active) = store.work_repo().active_run(&session.id)? {
        return Err(HostError::Conflict(format!(
            "the work session is already running run {}",
            active.run_id
        )));
    }
    let companion = require_companion(&store, session.companion_id.as_str())?;
    let run = new_run(
        uuid::Uuid::new_v4().to_string(),
        &session.id,
        instruction,
        RunState::Running,
    );
    store.work_repo().insert_run(&run)?;
    store
        .work_repo()
        .set_work_status(&session.id, WorkStatus::Active)?;
    // The backend pick: a task assignment onto an installed harness plugin
    // runs the session on the plugin hand; anything else keeps the rig agent.
    let failure = match hand::selected(&store, world.get_resource::<Providers>(), &companion) {
        Some((adapter, running)) => {
            let mut hands = std::mem::take(&mut *world.resource_mut::<hand::Hands>());
            let outcome = hand::start_run(
                world,
                &mut hands,
                &hand::RunStart {
                    session,
                    run: &run,
                    instruction,
                },
                &adapter,
                running,
            );
            *world.resource_mut::<hand::Hands>() = hands;
            outcome.err()
        }
        None => {
            let agent = ensure(world, &companion, &session.id, &run.run_id, instruction);
            match agent::ensure_model(world, agent) {
                Ok(()) => {
                    world.trigger(Deliver::new(
                        agent,
                        instruction.to_owned(),
                        DeliveryMode::Queue,
                    ));
                    None
                }
                Err(error) => Some(error),
            }
        }
    };
    if let Some(error) = failure {
        ene_domain::warn_if_err(fail_run(&store, &run, &error), "failed run record");
        let _written = store
            .work_repo()
            .set_work_status(&session.id, WorkStatus::Idle);
        if let Ok(Some(session)) = store.work_repo().work_session(&session.id) {
            let mut notifier = world.resource_mut::<Notifier>();
            notify_status(&mut notifier, &session);
            if let Ok(Some(failed)) = store.work_repo().run(&run.run_id) {
                notify_finished(&mut notifier, &session, &failed);
            }
        }
        return Err(error);
    }
    world.resource_mut::<Runs>().begin(&run.run_id);
    let session = require_session(&store, &session.id)?;
    {
        let mut notifier = world.resource_mut::<Notifier>();
        notify_status(&mut notifier, &session);
        notify_run(
            &mut notifier,
            &session,
            &run,
            w::WorkEventKind::RunStarted,
            w::work_event::Detail::RunStarted(Box::default()),
        );
    }
    Ok(run)
}

fn fail_run(store: &Store, run: &Run, error: &HostError) -> Result<()> {
    let mut failed = run.clone();
    failed.state = RunState::Failed;
    failed.failure = Some(Failure::new(error.code(), error.to_string()));
    Ok(store.work_repo().update_run(&failed)?)
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

pub(crate) fn send(world: &mut World, request: &w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let session = require_session(&store, &request.work_id)?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::Conflict(("work session is closed").into()));
    }
    let hash = ene_store::util::digest(format!("{}:{}", request.work_id, request.text).as_bytes());
    let agent = agent_of(world, &request.work_id);
    if !request.request_key.is_empty()
        && let Some((stored, request_id)) = receipt(
            agent.and_then(|entity| world.get::<Conversation>(entity)),
            world.get_resource::<Receipts>(),
            &request.work_id,
            &request.request_key,
        )
    {
        if stored != hash {
            return Err(HostError::AlreadyExists(
                ("this request key already accepted different content").into(),
            ));
        }
        return Ok(w::WorkAcceptance {
            request_id,
            ..Default::default()
        });
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    if let Some(run) = store.work_repo().active_run(&request.work_id)? {
        // A plugin hand parks mid-run input and delivers it as the next
        // prompt turn; the rig agent takes it inside its turn.
        if hand::owns(world.resource::<hand::Hands>(), &request.work_id) {
            hand::steer(
                &mut world.resource_mut::<hand::Hands>(),
                &request.work_id,
                &request.text,
            );
            world.resource_mut::<Runs>().touch(&run.run_id);
            return accepted(world, &store, request, &hash, request_id);
        }
        let Some(agent) = agent else {
            return Err(HostError::Unavailable(
                ("the work agent is not running").into(),
            ));
        };
        bind_work_agent(world.get_mut::<WorkAgent>(agent), &run.run_id);
        world.trigger(Deliver::new(
            agent,
            request.text.clone(),
            DeliveryMode::Queue,
        ));
        world.resource_mut::<Runs>().touch(&run.run_id);
        return accepted(world, &store, request, &hash, request_id);
    }
    let run = start_run(world, &session, &request.text)?;
    world.resource_mut::<Runs>().touch(&run.run_id);
    accepted(world, &store, request, &hash, request_id)
}

fn accepted(
    world: &mut World,
    store: &Store,
    request: &w::SendWorkRequest,
    hash: &[u8],
    request_id: String,
) -> Result<w::WorkAcceptance> {
    if !request.request_key.is_empty() {
        let agent = agent_of(world, &request.work_id);
        record_conversation_receipt(
            agent.and_then(|entity| world.get_mut::<Conversation>(entity)),
            &request.request_key,
            hash,
            &request_id,
        );
        world.resource_mut::<Receipts>().put(
            &request.work_id,
            &request.request_key,
            hash,
            &request_id,
        );
    }
    if let Ok(Some(session)) = store.work_repo().work_session(&request.work_id) {
        notify_status(&mut world.resource_mut::<Notifier>(), &session);
    }
    Ok(w::WorkAcceptance {
        request_id,
        ..Default::default()
    })
}

fn receipt(
    conversation: Option<&Conversation>,
    receipts: Option<&Receipts>,
    work_id: &str,
    request_key: &str,
) -> Option<(Vec<u8>, String)> {
    if let Some(conversation) = conversation {
        for (_, aside) in conversation
            .asides()
            .filter(|(_, aside)| aside.kind == "ene.work_accepted")
        {
            if aside.value.get("request_key").and_then(|key| key.as_str()) != Some(request_key) {
                continue;
            }
            let hash = aside
                .value
                .get("hash")?
                .as_array()?
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|byte| byte as u8)
                .collect();
            let request_id = aside.value.get("request_id")?.as_str()?;
            return Some((hash, request_id.to_owned()));
        }
    }
    receipts.and_then(|receipts| receipts.get(work_id, request_key))
}

fn record_conversation_receipt(
    conversation: Option<Mut<Conversation>>,
    request_key: &str,
    hash: &[u8],
    request_id: &str,
) {
    if let Some(mut conversation) = conversation {
        conversation.add_aside(rig_core::conversation::Aside::new(
            "ene.work_accepted",
            rig_core::conversation::Author::plugin("ene-host"),
            json!({
                "request_key": request_key,
                "request_id": request_id,
                "hash": hash,
                "accepted_at": now(),
            }),
        ));
    }
}

/// Stops the open run of a session. The stop is final at once: the work agent
/// is told to stop its turn, the run reads cancelled, and the parent is told.
/// Nothing waits for the agent's turn to unwind.
pub(crate) fn stop(world: &mut World, work_id: &str) -> Result<()> {
    stop_with(world, work_id, None)
}

pub(crate) fn stop_with(world: &mut World, work_id: &str, reason: Option<&str>) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if require_session(&store, work_id)?.status == WorkStatus::Closed {
        return Ok(());
    }
    let Some(run) = store.work_repo().active_run(work_id)? else {
        return Ok(());
    };
    if hand::owns(world.resource::<hand::Hands>(), work_id) {
        let runtime = world
            .resource::<crate::host::resources::HostRuntime>()
            .0
            .clone();
        let wake = world.get_resource::<rig_ecs::tasks::Wake>().cloned();
        let mut hands = std::mem::take(&mut *world.resource_mut::<hand::Hands>());
        {
            let mut calls = hand::CallDespawner::new(world);
            hand::stop(&mut hands, &mut calls, &runtime, wake.as_ref(), work_id);
        }
        *world.resource_mut::<hand::Hands>() = hands;
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
    stop_with(world, work_id, Some("the work session is closing"))?;
    store.work_repo().close_work(work_id)?;
    if let Ok(Some(session)) = store.work_repo().work_session(work_id) {
        notify_status(&mut world.resource_mut::<Notifier>(), &session);
    }
    let mut hands = std::mem::take(&mut *world.resource_mut::<hand::Hands>());
    {
        let mut calls = hand::CallDespawner::new(world);
        hand::release(&mut calls, &mut hands, work_id);
    }
    *world.resource_mut::<hand::Hands>() = hands;
    retire(world, work_id);
    Ok(())
}

#[cfg(test)]
mod tests;
