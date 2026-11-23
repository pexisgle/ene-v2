//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` survives runs and owns one backend: the long-lived
//! in-process rig agent, or the plugin hand (`hand`) a companion's ACP task
//! assignment picks. A `Run` is one instruction turn that rebinds the rig
//! agent to its run id. Only a closed session, a failed start, or a restart
//! retires the agent, never an ordinary run end.
mod hand;
#[cfg(test)]
mod hand_tests;
mod skills;
mod tools;

use super::{
    agent::{self, WorkAgent, WorkReports},
    db::{Result, Store},
    error::{Failure, HostError},
    mind::Companion,
    notify::Notifier,
    providers::Providers,
    util::now,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::Message;
use rig_ecs::prelude::*;
use serde_json::json;
use std::{collections::HashMap, time::Instant};

pub(crate) use ene_domain::{Run, RunState, WorkSession, WorkStatus};
pub(crate) use ene_work::{STALL_TIMEOUT, new_run};

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

fn agent_of(world: &mut World, work_id: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &WorkAgent)>();
    query
        .iter(world)
        .find(|(_, agent)| agent.work_id == work_id)
        .map(|(entity, _)| entity)
}

/// Rebinding keeps `report`, settlement and permissions on the active run.
fn bind_work_agent(work: Option<Mut<WorkAgent>>, run_id: &str) {
    if let Some(mut work) = work {
        work.run_id = run_id.to_owned();
    }
}

fn ensure(
    world: &mut World,
    companion: &Companion,
    work_id: &str,
    run_id: &str,
    instruction: &str,
) -> Entity {
    match agent_of(world, work_id) {
        Some(entity) => {
            bind_work_agent(world.get_mut::<WorkAgent>(entity), run_id);
            entity
        }
        None => agent::spawn_work_agent(world, companion, work_id, run_id, instruction),
    }
}

fn retire_entity(world: &mut World, entity: Entity) {
    world.trigger(Retire { entity });
    // The session-log observer despawns the agent after saving it. A host
    // without that plugin must still release the entity.
    if world.get_entity(entity).is_ok() {
        world.despawn(entity);
    }
}

fn retire(world: &mut World, work_id: &str) {
    let entities: Vec<Entity> = {
        let mut query = world.query::<(Entity, &WorkAgent)>();
        query
            .iter(world)
            .filter(|(_, agent)| agent.work_id == work_id)
            .map(|(entity, _)| entity)
            .collect()
    };
    for entity in entities {
        retire_entity(world, entity);
    }
}

/// Keeps one agent per open session after a restart and restarts the stall
/// clock of each open run. The active run's agent (else the first) wins.
fn restore(world: &mut World) {
    let store = world.resource::<Store>().clone();
    let agents: Vec<(Entity, String, String)> = {
        let mut query = world.query::<(Entity, &WorkAgent)>();
        query
            .iter(world)
            .map(|(entity, agent)| (entity, agent.work_id.clone(), agent.run_id.clone()))
            .collect()
    };
    let mut kept: HashMap<String, Entity> = HashMap::new();
    for (entity, work_id, run_id) in &agents {
        let active = match store.work_repo().active_run(work_id) {
            Ok(active) => active.map(|run| run.run_id),
            Err(error) => {
                if error.is_storage() {
                    super::fail(world, error.to_string());
                    return;
                }
                None
            }
        };
        let open = match store.work_repo().work_session(work_id) {
            Ok(session) => session.is_some_and(|session| session.status != WorkStatus::Closed),
            Err(error) => {
                if error.is_storage() {
                    super::fail(world, error.to_string());
                    return;
                }
                false
            }
        };
        if open && (active.as_deref() == Some(run_id.as_str()) || !kept.contains_key(work_id)) {
            kept.insert(work_id.clone(), *entity);
        }
    }
    for (entity, work_id, _) in agents {
        if kept.get(&work_id) != Some(&entity) {
            retire_entity(world, entity);
        }
    }
    let companions = match store.companion_repo().companions() {
        Ok(companions) => companions,
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
            }
            return;
        }
    };
    for companion in companions {
        let plugin =
            hand::selected(&store, world.get_resource::<Providers>(), &companion).is_some();
        let sessions = match store.work_repo().work_sessions(&companion.id) {
            Ok(sessions) => sessions,
            Err(error) => {
                if error.is_storage() {
                    super::fail(world, error.to_string());
                    return;
                }
                continue;
            }
        };
        for session in sessions {
            match store.work_repo().active_run(&session.id) {
                Ok(Some(run)) => {
                    if plugin {
                        // A restart lost the harness guest's session state, so an
                        // open plugin-hand run settles failed instead of keeping
                        // a stall clock that can never tick again.
                        hand::settle_restarts(world, run);
                    } else {
                        world.resource_mut::<Runs>().begin(&run.run_id);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                }
            }
        }
    }
}

pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        skills::register(app);
        super::web_search::register(app);
        app.init_resource::<Runs>()
            .init_resource::<Receipts>()
            .init_resource::<hand::Hands>()
            .add_systems(Startup, restore)
            .add_systems(
                Update,
                (hand::collect, detect_stalls).chain().after(AgentLoop),
            );
    }
}

pub(crate) fn list(store: &Store, companion: &str) -> Result<w::WorkViews> {
    require_companion(store, companion)?;
    let mut items = Vec::new();
    for session in store.work_repo().work_sessions(companion)? {
        items.push(view(store, &session.id)?);
    }
    Ok(w::WorkViews {
        items,
        ..Default::default()
    })
}

pub(crate) fn get(store: &Store, work_id: &str) -> Result<w::WorkView> {
    view(store, work_id)
}

fn view(store: &Store, work_id: &str) -> Result<w::WorkView> {
    let session = require_session(store, work_id)?;
    let runs = store.work_repo().runs(work_id)?;
    Ok(wire::work_view(&session, &runs))
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

pub(crate) fn start(world: &mut World, request: w::StartWorkRequest) -> Result<w::WorkSession> {
    let store = world.resource::<Store>().clone();
    let companion = require_companion(&store, &request.companion_id)?;
    if request.instruction.trim().is_empty() {
        return Err(HostError::Invalid(("an instruction is required").into()));
    }
    let client_id = binding_for(&store, &request)?;
    let session = WorkSession {
        id: uuid::Uuid::new_v4().to_string(),
        companion_id: companion.id.clone(),
        title: if request.title.trim().is_empty() {
            super::util::truncate(&request.instruction, 80)
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
        let _ = store
            .work_repo()
            .set_work_status(&session.id, WorkStatus::Closed);
        retire(world, &session.id);
        return Err(error);
    }
    let stored = require_session(&store, &session.id)?;
    Ok(wire::work_session(&stored))
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
    let companion = require_companion(&store, &session.companion_id)?;
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
                hand::RunStart {
                    session,
                    run: &run,
                    instruction,
                },
                adapter,
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
        let _ = fail_run(&store, &run, &error);
        let _ = store
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
    failed.failure = Some(Failure::new(error.code(), error.message()));
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
    super::util::validate_id(&request.client_id)?;
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

pub(crate) fn send(world: &mut World, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let session = require_session(&store, &request.work_id)?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::Conflict(("work session is closed").into()));
    }
    let hash = super::util::digest(format!("{}:{}", request.work_id, request.text).as_bytes());
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
    request: w::SendWorkRequest,
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
    let reason = reason.unwrap_or("the user stopped the work");
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
        world.trigger(StopTurn {
            entity: agent,
            reason: Some(reason.to_owned()),
        });
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

fn resolve_settle_outcome(
    reports: &WorkReports,
    run_id: &str,
    outcome: &TurnOutcome,
) -> (RunState, Option<String>) {
    let reported_failure = reports.failed.get(run_id).cloned();
    let reported_completed = reports.completed.get(run_id).cloned();
    match (reported_failure, reported_completed, outcome) {
        (Some(reason), _, _) => (RunState::Failed, Some(reason)),
        (None, Some(report), _) => (RunState::Completed, Some(report)),
        (None, None, TurnOutcome::Answered(message)) => {
            (RunState::Completed, message_text(message))
        }
        (None, None, TurnOutcome::Stopped(_)) => (RunState::Cancelled, None),
        (None, None, TurnOutcome::Failed(why)) => (
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    }
}

pub(crate) fn settle_run(world: &mut World, run_id: String, outcome: TurnOutcome) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.work_repo().run(&run_id) else {
        return;
    };
    if run.state.is_terminal() {
        return;
    }
    let (state, text) = resolve_settle_outcome(world.resource::<WorkReports>(), &run_id, &outcome);
    finish_run(world, run, state, text);
}

fn finish_run(world: &mut World, mut run: Run, state: RunState, text: Option<String>) {
    let store = world.resource::<Store>().clone();
    let active = match store.work_repo().active_run(&run.work_id) {
        Ok(active) => active,
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
            }
            return;
        }
    };
    let was_active = active.is_some_and(|active| active.run_id == run.run_id);
    let run_id = run.run_id.clone();
    run.state = state;
    run.pending_request_id = None;
    if state == RunState::Failed {
        run.failure = Some(Failure::new(
            "work_failed",
            text.clone().unwrap_or_else(|| "the run failed".to_owned()),
        ));
    }
    if let Err(error) = store.work_repo().update_run(&run) {
        super::fail(world, error.to_string());
        return;
    }
    super::client::computer::invalidate_run(world, &run_id);
    let session = match store.work_repo().work_session(&run.work_id) {
        Ok(session) => session,
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
            }
            return;
        }
    };
    if was_active && let Some(session) = &session {
        if session.status != WorkStatus::Closed {
            let _ = store
                .work_repo()
                .set_work_status(&session.id, WorkStatus::Idle);
        }
        if let Ok(Some(session)) = store.work_repo().work_session(&session.id) {
            {
                let mut notifier = world.resource_mut::<Notifier>();
                notify_status(&mut notifier, &session);
                notify_finished(&mut notifier, &session, &run);
            }
            if let Some(parent) = agent::agent_of(world, &session.companion_id) {
                let raw_report = text.unwrap_or_else(|| {
                    match state {
                        RunState::Cancelled => "作業を中断しました",
                        _ => "作業が終わりました",
                    }
                    .to_owned()
                });
                let report = if raw_report.starts_with('[') {
                    raw_report
                } else {
                    format!("[作業結果 (work_id: {})] {raw_report}", session.id)
                };
                let author = Author::agent(
                    agent::work_agent_id(&run_id),
                    Some(RequestId(run_id.clone())),
                );
                world
                    .trigger(Deliver::new(parent, report, DeliveryMode::Queue).with_author(author));
            }
        }
    } else if let Some(session) = &session {
        notify_finished(&mut world.resource_mut::<Notifier>(), session, &run);
    }
    if session
        .as_ref()
        .is_some_and(|session| session.status == WorkStatus::Closed)
    {
        retire(world, &run.work_id);
    }
    world.resource_mut::<Runs>().forget(&run.run_id);
    let reports = &mut world.resource_mut::<WorkReports>();
    reports.completed.remove(&run.run_id);
    reports.failed.remove(&run.run_id);
}

pub(super) fn note_activity(runs: &mut Runs, run_id: &str) {
    runs.touch(run_id);
}

/// Hands one report from a work run to its parent, as the run's own words.
fn inform(world: &mut World, companion: &str, run_id: &str, text: String, mode: DeliveryMode) {
    let Some(parent) = agent::agent_of(world, companion) else {
        return;
    };
    let author = Author::agent(agent::work_agent_id(run_id), None);
    world.trigger(Deliver::new(parent, text, mode).with_author(author));
}

fn detect_stalls(world: &mut World) {
    let due = world
        .resource_mut::<Runs>()
        .stalled(Instant::now(), STALL_TIMEOUT);
    for run_id in due {
        let store = world.resource::<Store>().clone();
        let Ok(Some(run)) = store.work_repo().run(&run_id) else {
            continue;
        };
        if run.state != RunState::Running {
            continue;
        }
        let Ok(Some(session)) = store.work_repo().work_session(&run.work_id) else {
            continue;
        };
        if session.status == WorkStatus::Closed {
            continue;
        }
        let Some(parent) = agent::agent_of(world, &session.companion_id) else {
            continue;
        };
        let notice = format!(
            "[System Notification: 作業「手」(run_id: {run_id}) からの報告が90秒間途絶しています。\
             現在の状況を確認してください。]"
        );
        world.trigger(
            Deliver::new(parent, notice, DeliveryMode::Inform)
                .with_author(Author::plugin("ene-host")),
        );
    }
}

fn message_text(message: &Message) -> Option<String> {
    let Message::Assistant(reply) = message else {
        return None;
    };
    let text = reply
        .content
        .iter()
        .filter_map(|item| match item {
            rig_core::completion::AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn notify_finished(notifier: &mut Notifier, session: &WorkSession, run: &Run) {
    notify_run(
        notifier,
        session,
        run,
        w::WorkEventKind::RunFinished,
        w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(run))),
    );
}

fn notify_status(notifier: &mut Notifier, session: &WorkSession) {
    notify_event(
        notifier,
        session,
        w::WorkEventKind::StatusChanged,
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: wire::work_status(session.status).into(),
            ..Default::default()
        })),
    );
}

fn notify_event(
    notifier: &mut Notifier,
    session: &WorkSession,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let event = wire::work_event(session, kind, detail);
    push_event(notifier, &session.companion_id, event);
}

fn notify_run(
    notifier: &mut Notifier,
    session: &WorkSession,
    run: &Run,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let mut event = wire::work_event(session, kind, detail);
    event.run_id = run.run_id.clone();
    push_event(notifier, &session.companion_id, event);
}

fn push_event(notifier: &mut Notifier, companion: &str, event: w::WorkEvent) {
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::WorkEvent(Box::new(event)),
    );
}

#[cfg(test)]
mod tests;
