//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` survives runs and owns one backend: the long-lived
//! in-process rig agent, or the plugin hand (`hand`) a companion's ACP task
//! assignment picks. A `Run` is one instruction turn that rebinds the rig
//! agent to its run id. Only a closed session, a failed start, or a restart
//! retires the agent, never an ordinary run end.
pub(crate) mod acp;
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
    util::now,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::Message;
use rig_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const STALL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkStatus {
    Idle,
    Active,
    Closed,
}

impl WorkStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunState {
    Starting,
    Running,
    WaitingInput,
    Completed,
    /// A stop reached its final state. A stored row that still spells a state
    /// this build no longer has decodes here.
    #[serde(alias = "cancelling", alias = "cancellation_unconfirmed")]
    Cancelled,
    Failed,
}

impl RunState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkSession {
    pub(crate) id: String,
    pub(crate) companion_id: String,
    pub(crate) title: String,
    pub(crate) status: WorkStatus,
    /// The client the work was commissioned from. Empty when no client was
    /// active at commissioning.
    pub(crate) client_id: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) run_id: String,
    pub(crate) work_id: String,
    pub(crate) instruction: String,
    pub(crate) state: RunState,
    /// The approval the run is waiting on, while it is `WaitingInput`.
    pub(crate) pending_request_id: Option<String>,
    pub(crate) failure: Option<Failure>,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
}

fn new_run(run_id: String, work_id: &str, instruction: &str, state: RunState) -> Run {
    Run {
        run_id,
        work_id: work_id.to_owned(),
        instruction: instruction.to_owned(),
        state,
        pending_request_id: None,
        failure: None,
        started_at: String::new(),
        updated_at: String::new(),
    }
}

#[derive(Resource, Default)]
struct Runs(HashMap<String, Live>);

struct Live {
    last: Instant,
    warned: bool,
}

impl Live {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            warned: false,
        }
    }
}

impl Runs {
    fn begin(&mut self, run_id: &str) {
        self.0.insert(run_id.to_owned(), Live::new());
    }

    fn touch(&mut self, run_id: &str) {
        let Some(live) = self.0.get_mut(run_id) else {
            return;
        };
        live.last = Instant::now();
        live.warned = false;
    }

    fn forget(&mut self, run_id: &str) {
        self.0.remove(run_id);
    }

    fn stalled(&mut self, now: Instant, threshold: Duration) -> Vec<String> {
        self.0
            .iter_mut()
            .filter_map(|(run_id, live)| {
                if live.warned || now.saturating_duration_since(live.last) < threshold {
                    return None;
                }
                live.warned = true;
                Some(run_id.clone())
            })
            .collect()
    }
}

/// `SendWork` receipts used while the session agent is not in the world; its
/// log holds the canonical copy.
#[derive(Resource, Default)]
struct Receipts(HashMap<String, (Vec<u8>, String)>);

impl Receipts {
    fn get(&self, work_id: &str, request_key: &str) -> Option<(Vec<u8>, String)> {
        self.0.get(&format!("{work_id}:{request_key}")).cloned()
    }

    fn put(&mut self, work_id: &str, request_key: &str, hash: &[u8], request_id: &str) {
        self.0.insert(
            format!("{work_id}:{request_key}"),
            (hash.to_vec(), request_id.to_owned()),
        );
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
fn bind(world: &mut World, entity: Entity, run_id: &str) {
    if let Some(mut work) = world.get_mut::<WorkAgent>(entity) {
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
            bind(world, entity, run_id);
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
        let active = match store.active_run(work_id) {
            Ok(active) => active.map(|run| run.run_id),
            Err(error) => {
                if error.is_storage() {
                    super::fail(world, error.to_string());
                    return;
                }
                None
            }
        };
        let open = match store.work_session(work_id) {
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
    let companions = match store.companions() {
        Ok(companions) => companions,
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
            }
            return;
        }
    };
    for companion in companions {
        let plugin = hand::selected(world, &companion).is_some();
        let sessions = match store.work_sessions(&companion.id) {
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
            match store.active_run(&session.id) {
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
    for session in store.work_sessions(companion)? {
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
    let runs = store.runs(work_id)?;
    Ok(wire::work_view(&session, &runs))
}

fn require_companion(store: &Store, id: &str) -> Result<Companion> {
    store
        .companion(id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))
}

fn require_session(store: &Store, id: &str) -> Result<WorkSession> {
    store
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
    store.insert_work_session(&session)?;
    if let Err(error) = start_run(world, &session, &request.instruction) {
        let _ = store.set_work_status(&session.id, WorkStatus::Closed);
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
    if let Some(active) = store.active_run(&session.id)? {
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
    store.insert_run(&run)?;
    store.set_work_status(&session.id, WorkStatus::Active)?;
    // The backend pick: a task assignment onto an installed harness plugin
    // runs the session on the plugin hand; anything else keeps the rig agent.
    let failure = match hand::selected(world, &companion) {
        Some((adapter, running)) => {
            hand::start_run(world, session, &run, instruction, adapter, running).err()
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
        let _ = store.set_work_status(&session.id, WorkStatus::Idle);
        if let Ok(Some(session)) = store.work_session(&session.id) {
            notify_status(world, &session);
            if let Ok(Some(failed)) = store.run(&run.run_id) {
                notify_finished(world, &session, &failed);
            }
        }
        return Err(error);
    }
    world.resource_mut::<Runs>().begin(&run.run_id);
    let session = require_session(&store, &session.id)?;
    notify_status(world, &session);
    notify_run(
        world,
        &session,
        &run,
        w::WorkEventKind::RunStarted,
        w::work_event::Detail::RunStarted(Box::default()),
    );
    Ok(run)
}

fn fail_run(store: &Store, run: &Run, error: &HostError) -> Result<()> {
    let mut failed = run.clone();
    failed.state = RunState::Failed;
    failed.failure = Some(Failure::new(error.code(), error.message()));
    store.update_run(&failed)
}

/// The client a new work session is commissioned from: the one the request
/// names, or the companion's active client when it names none. The session
/// keeps that client even after the companion moves to another one.
fn binding_for(store: &Store, request: &w::StartWorkRequest) -> Result<Option<String>> {
    let activation = store.activation(&request.companion_id)?;
    if request.client_id.is_empty() {
        return Ok(activation.and_then(|activation| activation.client_id));
    }
    super::util::validate_id(&request.client_id)?;
    store
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
        && let Some((stored, request_id)) =
            receipt(world, agent, &request.work_id, &request.request_key)
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
    if let Some(run) = store.active_run(&request.work_id)? {
        // A plugin hand parks mid-run input and delivers it as the next
        // prompt turn; the rig agent takes it inside its turn.
        if hand::owns(world, &request.work_id) {
            hand::steer(world, &request.work_id, &request.text);
            world.resource_mut::<Runs>().touch(&run.run_id);
            return accepted(world, &store, request, &hash, request_id);
        }
        let Some(agent) = agent else {
            return Err(HostError::Unavailable(
                ("the work agent is not running").into(),
            ));
        };
        bind(world, agent, &run.run_id);
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
        record_receipt(
            world,
            agent,
            &request.work_id,
            &request.request_key,
            hash,
            &request_id,
        );
    }
    if let Ok(Some(session)) = store.work_session(&request.work_id) {
        notify_status(world, &session);
    }
    Ok(w::WorkAcceptance {
        request_id,
        ..Default::default()
    })
}

fn receipt(
    world: &World,
    agent: Option<Entity>,
    work_id: &str,
    request_key: &str,
) -> Option<(Vec<u8>, String)> {
    if let Some(agent) = agent
        && let Some(conversation) = world.get::<Conversation>(agent)
    {
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
    world
        .get_resource::<Receipts>()
        .and_then(|receipts| receipts.get(work_id, request_key))
}

fn record_receipt(
    world: &mut World,
    agent: Option<Entity>,
    work_id: &str,
    request_key: &str,
    hash: &[u8],
    request_id: &str,
) {
    if let Some(agent) = agent
        && let Some(mut conversation) = world.get_mut::<Conversation>(agent)
    {
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
    world
        .resource_mut::<Receipts>()
        .put(work_id, request_key, hash, request_id);
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
    let Some(run) = store.active_run(work_id)? else {
        return Ok(());
    };
    let reason = reason.unwrap_or("the user stopped the work");
    if hand::owns(world, work_id) {
        hand::stop(world, work_id);
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
    store.close_work(work_id)?;
    if let Ok(Some(session)) = store.work_session(work_id) {
        notify_status(world, &session);
    }
    hand::release(world, work_id);
    retire(world, work_id);
    Ok(())
}

pub(crate) fn settle_run(world: &mut World, run_id: String, outcome: TurnOutcome) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.run(&run_id) else {
        return;
    };
    if run.state.is_terminal() {
        return;
    }
    let reported_failure = world.resource::<WorkReports>().failed.get(&run_id).cloned();
    let reported_completed = world
        .resource::<WorkReports>()
        .completed
        .get(&run_id)
        .cloned();
    let (state, text) = match (&reported_failure, &reported_completed, &outcome) {
        (Some(reason), _, _) => (RunState::Failed, Some(reason.clone())),
        (None, Some(report), _) => (RunState::Completed, Some(report.clone())),
        (None, None, TurnOutcome::Answered(message)) => {
            (RunState::Completed, message_text(message))
        }
        (None, None, TurnOutcome::Stopped(_)) => (RunState::Cancelled, None),
        (None, None, TurnOutcome::Failed(why)) => (
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    };
    finish_run(world, run, state, text);
}

fn finish_run(world: &mut World, mut run: Run, state: RunState, text: Option<String>) {
    let store = world.resource::<Store>().clone();
    let active = match store.active_run(&run.work_id) {
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
    if let Err(error) = store.update_run(&run) {
        super::fail(world, error.to_string());
        return;
    }
    super::client::computer::invalidate_run(world, &run_id);
    let session = match store.work_session(&run.work_id) {
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
            let _ = store.set_work_status(&session.id, WorkStatus::Idle);
        }
        if let Ok(Some(session)) = store.work_session(&session.id) {
            notify_status(world, &session);
            notify_finished(world, &session, &run);
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
        notify_finished(world, session, &run);
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

fn note_activity(world: &mut World, run_id: &str) {
    world.resource_mut::<Runs>().touch(run_id);
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
        let Ok(Some(run)) = store.run(&run_id) else {
            continue;
        };
        if run.state != RunState::Running {
            continue;
        }
        let Ok(Some(session)) = store.work_session(&run.work_id) else {
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

fn notify_finished(world: &mut World, session: &WorkSession, run: &Run) {
    notify_run(
        world,
        session,
        run,
        w::WorkEventKind::RunFinished,
        w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(run))),
    );
}

fn notify_status(world: &mut World, session: &WorkSession) {
    notify_event(
        world,
        session,
        w::WorkEventKind::StatusChanged,
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: wire::work_status(session.status).into(),
            ..Default::default()
        })),
    );
}

fn notify_event(
    world: &mut World,
    session: &WorkSession,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let event = wire::work_event(session, kind, detail);
    push_event(world, &session.companion_id, event);
}

fn notify_run(
    world: &mut World,
    session: &WorkSession,
    run: &Run,
    kind: w::WorkEventKind,
    detail: w::work_event::Detail,
) {
    let mut event = wire::work_event(session, kind, detail);
    event.run_id = run.run_id.clone();
    push_event(world, &session.companion_id, event);
}

fn push_event(world: &mut World, companion: &str, event: w::WorkEvent) {
    world.resource_mut::<Notifier>().push(
        Some(companion),
        None,
        w::notification::Event::WorkEvent(Box::new(event)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        (root, store)
    }

    fn session(store: &Store) -> WorkSession {
        let session = WorkSession {
            id: "w1".into(),
            companion_id: "ene".into(),
            title: "task".into(),
            status: WorkStatus::Active,
            client_id: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        store.insert_work_session(&session).unwrap();
        session
    }

    fn run(store: &Store, run_id: &str, state: RunState) {
        let run = new_run(run_id.into(), "w1", "do it", state);
        store.insert_run(&run).unwrap();
    }

    fn world(store: Store) -> World {
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        world.insert_resource(WorkReports::default());
        world.insert_resource(Runs::default());
        world.insert_resource(hand::Hands::default());
        world
    }

    fn agent(world: &mut World, run_id: &str) -> Entity {
        world
            .spawn(WorkAgent {
                work_id: "w1".into(),
                run_id: run_id.into(),
                companion_id: "ene".into(),
            })
            .id()
    }

    fn state(store: &Store, run_id: &str) -> RunState {
        store.run(run_id).unwrap().unwrap().state
    }

    #[test]
    fn a_session_keeps_one_agent_across_runs_and_retires_it_with_the_session() {
        let (_root, store) = store();
        session(&store);
        run(&store, "run-a", RunState::Running);
        let mut world = world(store.clone());
        let kept = agent(&mut world, "run-a");

        bind(&mut world, kept, "run-b");
        assert_eq!(agent_of(&mut world, "w1"), Some(kept));
        assert_eq!(world.get::<WorkAgent>(kept).unwrap().run_id, "run-b");

        settle_run(&mut world, "run-a".into(), TurnOutcome::Stopped(None));
        assert_eq!(state(&store, "run-a"), RunState::Cancelled);
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Idle
        );
        assert_eq!(agent_of(&mut world, "w1"), Some(kept));

        run(&store, "run-b", RunState::Running);
        let stale = agent(&mut world, "run-a");
        store.set_work_status("w1", WorkStatus::Active).unwrap();
        restore(&mut world);
        assert_eq!(agent_of(&mut world, "w1"), Some(kept));
        assert!(world.get::<WorkAgent>(stale).is_none());

        store.set_work_status("w1", WorkStatus::Closed).unwrap();
        restore(&mut world);
        assert!(agent_of(&mut world, "w1").is_none());
    }

    #[test]
    fn a_stall_is_warned_once_and_reset_by_activity() {
        let mut runs = Runs::default();
        runs.begin("run-a");
        let now = Instant::now();
        runs.0.get_mut("run-a").unwrap().last = now - STALL_TIMEOUT - Duration::from_secs(1);
        assert_eq!(runs.stalled(now, STALL_TIMEOUT), vec!["run-a".to_owned()]);
        assert!(runs.stalled(now, STALL_TIMEOUT).is_empty());
        runs.touch("run-a");
        assert!(runs.stalled(now, STALL_TIMEOUT).is_empty());
        assert_eq!(
            runs.stalled(now + STALL_TIMEOUT + Duration::from_secs(1), STALL_TIMEOUT),
            vec!["run-a".to_owned()],
            "activity resets the silence window"
        );
    }

    #[test]
    fn stop_then_close_settles_the_run_and_releases_the_agent() {
        let (_root, store) = store();
        session(&store);
        run(&store, "run-a", RunState::Running);
        let mut world = world(store.clone());
        let kept = agent(&mut world, "run-a");

        stop(&mut world, "w1").unwrap();
        assert_eq!(state(&store, "run-a"), RunState::Cancelled);
        assert_eq!(agent_of(&mut world, "w1"), Some(kept));

        close(&mut world, "w1").unwrap();
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Closed
        );
        assert!(agent_of(&mut world, "w1").is_none());
    }

    #[test]
    fn a_completion_that_races_a_stop_stays_cancelled() {
        let (_root, store) = store();
        session(&store);
        run(&store, "run-a", RunState::Running);
        let mut world = world(store.clone());

        stop(&mut world, "w1").unwrap();
        settle_run(
            &mut world,
            "run-a".into(),
            TurnOutcome::Answered(Message::assistant("done")),
        );
        assert_eq!(state(&store, "run-a"), RunState::Cancelled);

        run(&store, "run-b", RunState::Running);
        settle_run(
            &mut world,
            "run-b".into(),
            TurnOutcome::Answered(Message::assistant("done")),
        );
        assert_eq!(state(&store, "run-b"), RunState::Completed);
    }
}
