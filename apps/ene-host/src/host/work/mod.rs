//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` survives runs and owns one long-lived backend (the rig agent
//! or one ACP process, see [`acp`]); a `Run` is one instruction turn that
//! rebinds the session agent to its run id. Only a closed session, a failed
//! start, or a restart retires the agent, never an ordinary run end.
mod acp;
mod tools;

use self::acp::{AcpBackends, AcpEvents};
use super::{
    agent::{self, WorkAgent, WorkReports},
    db::{Result, Store, now},
    error::{Failure, HostError},
    mind::{Companion, Role},
    notify::Notifier,
    settings::{AcpConnection, ConnectionConfig},
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
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const FORCE_GRACE: Duration = Duration::from_secs(5);
const ACP_UPDATE_INTERVAL: Duration = Duration::from_secs(2);

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

    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_value(text.into()).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunState {
    Starting,
    Running,
    WaitingInput,
    Cancelling,
    Completed,
    Cancelled,
    CancellationUnconfirmed,
    Failed,
}

impl RunState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::CancellationUnconfirmed => "cancellation_unconfirmed",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_value(text.into()).ok()
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::CancellationUnconfirmed | Self::Failed
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEvent {
    Start,
    Stop,
    Complete,
    CancelConfirmed,
    CancelUnconfirmed,
    Fail,
    Resume,
}

pub(crate) fn transition(state: RunState, event: RunEvent) -> Option<RunState> {
    use RunEvent::*;
    use RunState::*;
    Some(match (state, event) {
        (Starting, Start) => Running,
        (Starting | Running | WaitingInput, Stop) => Cancelling,
        (Starting | Running | WaitingInput, Complete) => Completed,
        (Starting | Running | WaitingInput | Cancelling, CancelConfirmed) => Cancelled,
        (Starting | Running | WaitingInput | Cancelling, CancelUnconfirmed) => {
            CancellationUnconfirmed
        }
        (Starting | Running | WaitingInput | Cancelling, Fail) => Failed,
        (WaitingInput, Resume) => Running,
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkSession {
    pub(crate) id: String,
    pub(crate) companion_id: String,
    pub(crate) title: String,
    pub(crate) status: WorkStatus,
    /// The client the work was commissioned from, and the generation it was
    /// active in. Empty when no client was active at commissioning.
    pub(crate) client_id: Option<String>,
    pub(crate) activation_revision: u64,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) run_id: String,
    pub(crate) work_id: String,
    pub(crate) instruction: String,
    pub(crate) state: RunState,
    pub(crate) pending_request_id: Option<String>,
    pub(crate) failure: Option<Failure>,
    pub(crate) cancellation_requested_at: Option<String>,
    pub(crate) cancellation_reason: Option<String>,
    pub(crate) unverified_pids: Vec<u32>,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
}

impl Run {
    pub(crate) fn detail(&self) -> RunDetail<'_> {
        RunDetail {
            failure: self.failure.as_ref(),
            pending_request_id: self.pending_request_id.as_deref(),
            cancellation_requested_at: self.cancellation_requested_at.as_deref(),
            cancellation_reason: self.cancellation_reason.as_deref(),
            unverified_pids: &self.unverified_pids,
        }
    }
}

pub(crate) struct RunDetail<'a> {
    pub(crate) failure: Option<&'a Failure>,
    pub(crate) pending_request_id: Option<&'a str>,
    pub(crate) cancellation_requested_at: Option<&'a str>,
    pub(crate) cancellation_reason: Option<&'a str>,
    pub(crate) unverified_pids: &'a [u32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inquiry {
    pub(crate) request_id: String,
    pub(crate) work_id: String,
    pub(crate) run_id: String,
    pub(crate) prompt: String,
    pub(crate) choices: Vec<String>,
    pub(crate) answer: Option<String>,
    pub(crate) created_at: String,
    pub(crate) resolved_at: Option<String>,
}

impl Inquiry {
    pub(crate) fn is_pending(&self) -> bool {
        self.resolved_at.is_none()
    }
}

fn new_run(run_id: String, work_id: &str, instruction: &str, state: RunState) -> Run {
    Run {
        run_id,
        work_id: work_id.to_owned(),
        instruction: instruction.to_owned(),
        state,
        pending_request_id: None,
        failure: None,
        cancellation_requested_at: None,
        cancellation_reason: None,
        unverified_pids: Vec::new(),
        started_at: String::new(),
        updated_at: String::new(),
    }
}

#[derive(Resource, Default)]
struct Runs(HashMap<String, Live>);

struct Live {
    last: Instant,
    forwarded: Instant,
    warned: bool,
    stop: Option<Stop>,
}

struct Stop {
    reason: String,
    deadline: Instant,
    forced: bool,
}

impl Live {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last: now,
            forwarded: now,
            warned: false,
            stop: None,
        }
    }
}

impl Runs {
    fn begin(&mut self, run_id: &str) {
        self.0.insert(run_id.to_owned(), Live::new());
    }

    fn touch(&mut self, run_id: &str) {
        let live = self.0.entry(run_id.to_owned()).or_insert_with(Live::new);
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

    fn arm(&mut self, run_id: &str, reason: &str) {
        let live = self.0.entry(run_id.to_owned()).or_insert_with(Live::new);
        live.stop = Some(Stop {
            reason: reason.to_owned(),
            deadline: Instant::now() + STOP_TIMEOUT,
            forced: false,
        });
    }

    fn set_stop(&mut self, run_id: &str, stop: Option<Stop>) {
        if let Some(live) = self.0.get_mut(run_id) {
            live.stop = stop;
        }
    }

    fn due_stops(&mut self, now: Instant) -> Vec<(String, Stop)> {
        self.0
            .iter_mut()
            .filter_map(|(run_id, live)| {
                live.stop
                    .take_if(|stop| stop.deadline <= now)
                    .map(|stop| (run_id.clone(), stop))
            })
            .collect()
    }
}

/// `SendWork` receipts for ACP sessions; the rig backend keeps the canonical
/// copy in its session-log aside. Not persisted: a restart forgets them.
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
        let active = store
            .active_run(work_id)
            .ok()
            .flatten()
            .map(|run| run.run_id);
        let open = store
            .work_session(work_id)
            .ok()
            .flatten()
            .is_some_and(|session| session.status != WorkStatus::Closed);
        if open && (active.as_deref() == Some(run_id.as_str()) || !kept.contains_key(work_id)) {
            kept.insert(work_id.clone(), *entity);
        }
    }
    for (entity, work_id, _) in agents {
        if kept.get(&work_id) != Some(&entity) {
            retire_entity(world, entity);
        }
    }
    for companion in store.companions().unwrap_or_default() {
        for session in store.work_sessions(&companion.id).unwrap_or_default() {
            if let Ok(Some(run)) = store.active_run(&session.id) {
                world.resource_mut::<Runs>().begin(&run.run_id);
            }
        }
    }
}

pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        app.init_resource::<Runs>()
            .init_resource::<Receipts>()
            .init_resource::<AcpEvents>()
            .init_resource::<AcpBackends>()
            .add_systems(Startup, restore)
            .add_systems(
                Update,
                (acp::pump, expire_stops, detect_stalls)
                    .chain()
                    .after(AgentLoop),
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
    let inquiries = store.pending_inquiries(work_id)?;
    Ok(wire::work_view(&session, &runs, &inquiries))
}

fn require_companion(store: &Store, id: &str) -> Result<Companion> {
    store
        .companion(id)?
        .ok_or_else(|| HostError::not_found("companion not found"))
}

fn require_session(store: &Store, id: &str) -> Result<WorkSession> {
    store
        .work_session(id)?
        .ok_or_else(|| HostError::not_found("work session not found"))
}

fn require_inquiry(store: &Store, request_id: &str, work_id: &str) -> Result<Inquiry> {
    let inquiry = store
        .inquiry(request_id)?
        .ok_or_else(|| HostError::not_found("inquiry not found"))?;
    if inquiry.work_id != work_id {
        return Err(HostError::invalid(
            "the inquiry belongs to another work session",
        ));
    }
    Ok(inquiry)
}

pub(crate) fn start(world: &mut World, request: w::StartWorkRequest) -> Result<w::WorkSession> {
    let store = world.resource::<Store>().clone();
    let companion = require_companion(&store, &request.companion_id)?;
    if request.instruction.trim().is_empty() {
        return Err(HostError::invalid("an instruction is required"));
    }
    let (client_id, activation_revision) = binding_for(&store, &request)?;
    let session = WorkSession {
        id: uuid::Uuid::new_v4().to_string(),
        companion_id: companion.id.clone(),
        title: if request.title.trim().is_empty() {
            super::db::truncate(&request.instruction, 80)
        } else {
            request.title.clone()
        },
        status: WorkStatus::Idle,
        client_id,
        activation_revision,
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_work_session(&session)?;
    if let Err(error) = start_run(world, &session, &request.instruction) {
        let _ = store.set_work_status(&session.id, WorkStatus::Closed);
        retire(world, &session.id);
        acp::shutdown(world, &session.id);
        return Err(error);
    }
    let stored = require_session(&store, &session.id)?;
    Ok(wire::work_session(&stored))
}

enum Backend {
    Rig,
    /// `None` means the same session's ACP process is already running.
    Acp(Option<AcpConnection>),
}

fn plan_backend(
    world: &mut World,
    store: &Store,
    companion: &Companion,
    work_id: &str,
) -> Result<Backend> {
    if world.resource::<AcpBackends>().0.contains_key(work_id) {
        return Ok(Backend::Acp(None));
    }
    if agent_of(world, work_id).is_some() {
        return Ok(Backend::Rig);
    }
    let Some(assignment) = companion.model(Role::Task) else {
        return Ok(Backend::Rig);
    };
    let Some(connection) = store.connection(&assignment.connection_id)? else {
        return Ok(Backend::Rig);
    };
    let ConnectionConfig::Acp(config) = connection.config else {
        return Ok(Backend::Rig);
    };
    if !connection.enabled {
        return Err(HostError::unavailable(
            "the assigned ACP connection is disabled",
        ));
    }
    if config.command.trim().is_empty() {
        return Err(HostError::unavailable("the ACP connection has no command"));
    }
    // A session that already ran on ACP keeps its context in that process.
    if !store.runs(work_id)?.is_empty() {
        return Err(HostError::unavailable(
            "the ACP agent of this work session is no longer running",
        ));
    }
    Ok(Backend::Acp(Some(config)))
}

pub(crate) fn start_run(
    world: &mut World,
    session: &WorkSession,
    instruction: &str,
) -> Result<Run> {
    let store = world.resource::<Store>().clone();
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    if instruction.trim().is_empty() {
        return Err(HostError::invalid("an instruction is required"));
    }
    if let Some(active) = store.active_run(&session.id)? {
        return Err(HostError::conflict(format!(
            "the work session is already running run {}",
            active.run_id
        )));
    }
    let companion = require_companion(&store, &session.companion_id)?;
    let backend = plan_backend(world, &store, &companion, &session.id)?;
    let starting = matches!(backend, Backend::Acp(Some(_)));
    let state = if starting {
        RunState::Starting
    } else {
        RunState::Running
    };
    let run_id = uuid::Uuid::new_v4().to_string();
    let run = new_run(run_id, &session.id, instruction, state);
    store.insert_run(&run)?;
    store.set_work_status(&session.id, WorkStatus::Active)?;
    let failure = match backend {
        Backend::Rig => {
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
        Backend::Acp(connection) => {
            let prompted = match connection {
                Some(connection) => acp::spawn(world, &session.id, &connection).map(|handle| {
                    let prompted = handle.prompt(&run.run_id, instruction);
                    world
                        .resource_mut::<AcpBackends>()
                        .0
                        .insert(session.id.clone(), handle);
                    prompted
                }),
                None => Ok(world
                    .resource::<AcpBackends>()
                    .0
                    .get(&session.id)
                    .is_some_and(|handle| handle.prompt(&run.run_id, instruction))),
            };
            match prompted {
                Ok(true) => None,
                Ok(false) => {
                    acp::shutdown(world, &session.id);
                    Some(HostError::unavailable(
                        "the ACP agent is not accepting prompts",
                    ))
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
    failed.state = transition(run.state, RunEvent::Fail).unwrap_or(RunState::Failed);
    failed.failure = Some(Failure::new(error.code(), error.message()));
    store.update_run(&failed)
}

fn binding_for(store: &Store, request: &w::StartWorkRequest) -> Result<(Option<String>, u64)> {
    let activation = store.activation(&request.companion_id)?;
    if request.client_id.is_empty() {
        return Ok(activation
            .map(|activation| (activation.client_id, activation.revision))
            .unwrap_or((None, 0)));
    }
    super::db::validate_id(&request.client_id)?;
    store
        .client(&request.client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    let Some(activation) = activation else {
        return Err(HostError::conflict(
            "the companion has no active client to bind the session to",
        ));
    };
    if activation.client_id.as_deref() != Some(request.client_id.as_str()) {
        return Err(HostError::conflict(
            "the requested client is not the active client of this companion",
        ));
    }
    Ok((activation.client_id, activation.revision))
}

pub(crate) fn send(world: &mut World, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let session = require_session(&store, &request.work_id)?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    let inquiry_id = request.request_id.clone().unwrap_or_default();
    let hash = super::db::digest(
        format!("{}:{}:{}", request.work_id, request.text, inquiry_id).as_bytes(),
    );
    let agent = agent_of(world, &request.work_id);
    if !request.request_key.is_empty()
        && let Some((stored, request_id)) =
            receipt(world, agent, &request.work_id, &request.request_key)
    {
        if stored != hash {
            return Err(HostError::already_exists(
                "this request key already accepted different content",
            ));
        }
        return Ok(w::WorkAcceptance {
            request_id,
            ..Default::default()
        });
    }
    let inquiry = if inquiry_id.is_empty() {
        None
    } else {
        Some(require_inquiry(&store, &inquiry_id, &request.work_id)?)
    };
    let request_id = if inquiry_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        inquiry_id.clone()
    };
    if let Some(mut run) = store.active_run(&request.work_id)? {
        if run.state.is_terminal() {
            return Err(HostError::conflict("work session has no active run"));
        }
        if world
            .resource::<AcpBackends>()
            .0
            .contains_key(&request.work_id)
        {
            return Err(HostError::conflict(
                "the ACP agent is running a prompt; wait for it to finish before sending more input",
            ));
        }
        let Some(agent) = agent else {
            return Err(HostError::unavailable("the work agent is not running"));
        };
        bind(world, agent, &run.run_id);
        if let Some(inquiry) = &inquiry {
            if inquiry.run_id != run.run_id {
                return Err(HostError::conflict(
                    "the inquiry belongs to a run that is no longer active",
                ));
            }
            store.resolve_inquiry(&inquiry_id, &request.text)?;
            resume(&mut run);
            store.update_run(&run)?;
            notify_active(world, &session, &run);
        }
        world.trigger(Deliver::new(
            agent,
            request.text.clone(),
            DeliveryMode::Steer,
        ));
        world.resource_mut::<Runs>().touch(&run.run_id);
        return accepted(world, &store, request, &hash, request_id);
    }
    let run = start_run(world, &session, &request.text)?;
    if let Some(inquiry) = &inquiry {
        store.resolve_inquiry(&inquiry.request_id, &request.text)?;
    }
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

pub(crate) fn answer_inquiry(
    world: &mut World,
    request: w::AnswerInquiryRequest,
) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let inquiry = require_inquiry(&store, &request.request_id, &request.work_id)?;
    let session = require_session(&store, &request.work_id)?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    if world
        .resource::<AcpBackends>()
        .0
        .contains_key(&request.work_id)
    {
        return Err(HostError::unavailable(
            "ACP work sessions do not report inquiries",
        ));
    }
    let mut run = store
        .active_run(&request.work_id)?
        .ok_or_else(|| HostError::conflict("work session has no active run"))?;
    if run.run_id != inquiry.run_id {
        return Err(HostError::conflict(
            "the inquiry belongs to a run that is no longer active",
        ));
    }
    let agent = agent_of(world, &request.work_id)
        .ok_or_else(|| HostError::unavailable("the work agent is not running"))?;
    bind(world, agent, &run.run_id);
    store.resolve_inquiry(&inquiry.request_id, &request.answer)?;
    resume(&mut run);
    store.update_run(&run)?;
    world.resource_mut::<Runs>().touch(&run.run_id);
    world.trigger(Deliver::new(
        agent,
        format!("ユーザーからの回答: {}", request.answer),
        DeliveryMode::Steer,
    ));
    notify_active(world, &session, &run);
    Ok(w::WorkAcceptance {
        request_id: request.request_id,
        ..Default::default()
    })
}

fn resume(run: &mut Run) {
    run.state = transition(run.state, RunEvent::Resume).unwrap_or(RunState::Running);
    run.pending_request_id = None;
}

pub(crate) fn stop(world: &mut World, work_id: &str) -> Result<()> {
    stop_with(world, work_id, None)
}

pub(crate) fn stop_with(world: &mut World, work_id: &str, reason: Option<&str>) -> Result<()> {
    let store = world.resource::<Store>().clone();
    if require_session(&store, work_id)?.status == WorkStatus::Closed {
        return Ok(());
    }
    let Some(mut run) = store.active_run(work_id)? else {
        return Ok(());
    };
    if run.state.is_terminal() {
        return Ok(());
    }
    let reason = reason.unwrap_or("the user stopped the work").to_owned();
    let already_cancelling = run.state == RunState::Cancelling;
    run.state = transition(run.state, RunEvent::Stop).unwrap_or(RunState::Cancelling);
    if run.cancellation_requested_at.is_none() {
        run.cancellation_requested_at = Some(now());
    }
    run.cancellation_reason = Some(reason.clone());
    store.update_run(&run)?;
    if !acp::cancel(world, work_id, &run.run_id)
        && let Some(agent) = agent_of(world, work_id)
    {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some(reason.clone()),
        });
    }
    if !already_cancelling {
        world.resource_mut::<Runs>().arm(&run.run_id, &reason);
    }
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
    retire(world, work_id);
    acp::shutdown(world, work_id);
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
    let (state, text) = match (&reported_failure, &outcome) {
        (Some(reason), _) => (
            transition(run.state, RunEvent::Fail).unwrap_or(RunState::Failed),
            Some(reason.clone()),
        ),
        (None, TurnOutcome::Answered(message)) => {
            let report = world
                .resource::<WorkReports>()
                .completed
                .get(&run_id)
                .cloned();
            (
                transition(run.state, RunEvent::Complete).unwrap_or(RunState::Completed),
                report.or_else(|| message_text(message)),
            )
        }
        (None, TurnOutcome::Stopped(_)) => (
            transition(run.state, RunEvent::CancelConfirmed).unwrap_or(RunState::Cancelled),
            None,
        ),
        (None, TurnOutcome::Failed(why)) => (
            transition(run.state, RunEvent::Fail).unwrap_or(RunState::Failed),
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    };
    finish_run(world, run, state, text, None);
}

pub(crate) fn record_forced_stop(
    world: &mut World,
    run_id: &str,
    confirmed: bool,
    pids: Vec<u32>,
    reason: &str,
) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.run(run_id) else {
        return;
    };
    if run.state.is_terminal() {
        world.resource_mut::<Runs>().set_stop(run_id, None);
        return;
    }
    let event = if confirmed {
        RunEvent::CancelConfirmed
    } else {
        RunEvent::CancelUnconfirmed
    };
    let state = transition(run.state, event).unwrap_or(if confirmed {
        RunState::Cancelled
    } else {
        RunState::CancellationUnconfirmed
    });
    let text = (state == RunState::CancellationUnconfirmed).then(|| {
        format!("作業を停止しましたが、完全に停止したか確認できませんでした（{reason}）。")
    });
    finish_run(world, run, state, text, Some((reason.to_owned(), pids)));
}

pub(crate) fn mark_run_running(world: &mut World, run_id: &str) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(mut run)) = store.run(run_id) else {
        return;
    };
    let Some(state) = transition(run.state, RunEvent::Start) else {
        return;
    };
    run.state = state;
    if store.update_run(&run).is_err() {
        return;
    }
    if let Ok(Some(session)) = store.work_session(&run.work_id) {
        notify_status(world, &session);
    }
}

fn finish_run(
    world: &mut World,
    mut run: Run,
    state: RunState,
    text: Option<String>,
    cancellation: Option<(String, Vec<u32>)>,
) {
    let store = world.resource::<Store>().clone();
    let active = store.active_run(&run.work_id).ok().flatten();
    let was_active = active.is_some_and(|active| active.run_id == run.run_id);
    let run_id = run.run_id.clone();
    run.state = state;
    run.pending_request_id = None;
    match state {
        RunState::Failed => {
            run.failure = Some(Failure::new(
                "work_failed",
                text.clone().unwrap_or_else(|| "the run failed".to_owned()),
            ));
        }
        RunState::Cancelled => run.unverified_pids.clear(),
        RunState::CancellationUnconfirmed => {
            let (reason, pids) = cancellation
                .unwrap_or_else(|| ("the stop was not confirmed".to_owned(), Vec::new()));
            run.cancellation_reason = Some(reason);
            run.unverified_pids = pids;
        }
        _ => {}
    }
    if let Err(error) = store.update_run(&run) {
        super::fail(world, error.to_string());
        return;
    }
    let session = store.work_session(&run.work_id).ok().flatten();
    if was_active && let Some(session) = &session {
        if session.status != WorkStatus::Closed {
            let _ = store.set_work_status(&session.id, WorkStatus::Idle);
        }
        if let Ok(Some(session)) = store.work_session(&session.id) {
            notify_status(world, &session);
            notify_finished(world, &session, &run);
            if let Some(parent) = agent::agent_of(world, &session.companion_id) {
                let report = text.unwrap_or_else(|| {
                    match state {
                        RunState::Cancelled => "作業を中断しました",
                        _ => "作業が終わりました",
                    }
                    .to_owned()
                });
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
        acp::shutdown(world, &run.work_id);
    }
    world.resource_mut::<Runs>().forget(&run.run_id);
    let reports = &mut world.resource_mut::<WorkReports>();
    reports.completed.remove(&run.run_id);
    reports.failed.remove(&run.run_id);
}

pub(crate) fn acp_update(world: &mut World, work_id: &str, run_id: &str, text: &str) {
    let forward = {
        let mut runs = world.resource_mut::<Runs>();
        runs.touch(run_id);
        let live = runs.0.get_mut(run_id).expect("touched above");
        let now = Instant::now();
        let due = now.saturating_duration_since(live.forwarded) >= ACP_UPDATE_INTERVAL;
        if due {
            live.forwarded = now;
        }
        due
    };
    let text = text.trim();
    if !forward || text.is_empty() {
        return;
    }
    let store = world.resource::<Store>().clone();
    let Ok(Some(session)) = store.work_session(work_id) else {
        return;
    };
    let Some(parent) = agent::agent_of(world, &session.companion_id) else {
        return;
    };
    let author = Author::agent(agent::work_agent_id(run_id), None);
    world.trigger(
        Deliver::new(
            parent,
            format!("[作業報告] {}", super::db::truncate(text, 500)),
            DeliveryMode::Inform,
        )
        .with_author(author),
    );
}

pub(crate) fn acp_closed(
    world: &mut World,
    work_id: &str,
    run_id: Option<String>,
    error: Option<String>,
    confirmed: bool,
    pids: Vec<u32>,
) {
    world.resource_mut::<AcpBackends>().0.remove(work_id);
    let Some(run_id) = run_id else {
        return;
    };
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.run(&run_id) else {
        return;
    };
    if run.state.is_terminal() {
        world.resource_mut::<Runs>().set_stop(&run_id, None);
        return;
    }
    if run.state == RunState::Cancelling {
        record_forced_stop(
            world,
            &run_id,
            confirmed,
            pids,
            "the ACP connection ended before the cancellation was acknowledged",
        );
    } else {
        settle_run(
            world,
            run_id,
            TurnOutcome::Failed(error.unwrap_or_else(|| "the ACP agent connection ended".into())),
        );
    }
}

fn note_activity(world: &mut World, run_id: &str) {
    world.resource_mut::<Runs>().touch(run_id);
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

fn expire_stops(world: &mut World) {
    let now = Instant::now();
    let due = world.resource_mut::<Runs>().due_stops(now);
    for (run_id, mut stop) in due {
        let store = world.resource::<Store>().clone();
        let Ok(Some(run)) = store.run(&run_id) else {
            continue;
        };
        if run.state.is_terminal() {
            continue;
        }
        if stop.forced {
            let pids = acp::pids(world, &run.work_id);
            let reason = format!("{} (the process group did not exit)", stop.reason);
            record_forced_stop(world, &run_id, false, pids, &reason);
        } else if !acp::force_kill(world, &run.work_id) {
            let reason = format!("{} (no process group to kill)", stop.reason);
            record_forced_stop(world, &run_id, false, Vec::new(), &reason);
        } else {
            stop.forced = true;
            stop.deadline = now + FORCE_GRACE;
            world.resource_mut::<Runs>().set_stop(&run_id, Some(stop));
        }
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

fn active_status() -> w::work_event::Detail {
    w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
        status: w::WorkStatus::Active.into(),
        ..Default::default()
    }))
}

fn notify_active(world: &mut World, session: &WorkSession, run: &Run) {
    notify_run(
        world,
        session,
        run,
        w::WorkEventKind::StatusChanged,
        active_status(),
    );
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
            activation_revision: 0,
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
        world.insert_resource(AcpBackends::default());
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
    fn a_stop_is_only_classified_cancelled_when_it_is_confirmed() {
        let (_root, store) = store();
        session(&store);
        run(&store, "run-a", RunState::Cancelling);
        let mut world = world(store.clone());

        world
            .resource_mut::<Runs>()
            .arm("run-a", "the user stopped the work");
        let due = world
            .resource_mut::<Runs>()
            .due_stops(Instant::now() + STOP_TIMEOUT + Duration::from_secs(1));
        assert_eq!(due.len(), 1);
        assert!(!due[0].1.forced);

        record_forced_stop(&mut world, "run-a", false, vec![4242], "not exited");
        let stored = store.run("run-a").unwrap().unwrap();
        assert_eq!(stored.state, RunState::CancellationUnconfirmed);
        assert_eq!(stored.unverified_pids, vec![4242]);
        assert!(
            stored
                .cancellation_reason
                .as_deref()
                .is_some_and(|r| r.contains("not exited"))
        );

        run(&store, "run-b", RunState::Cancelling);
        record_forced_stop(&mut world, "run-b", true, Vec::new(), "killed");
        assert_eq!(state(&store, "run-b"), RunState::Cancelled);
    }

    #[test]
    fn stop_then_close_settles_the_run_and_releases_the_agent() {
        let (_root, store) = store();
        session(&store);
        run(&store, "run-a", RunState::Running);
        let mut world = world(store.clone());
        let kept = agent(&mut world, "run-a");

        stop(&mut world, "w1").unwrap();
        let run = store.run("run-a").unwrap().unwrap();
        assert_eq!(run.state, RunState::Cancelling);
        assert_eq!(
            run.cancellation_reason.as_deref(),
            Some("the user stopped the work")
        );
        assert!(run.cancellation_requested_at.is_some());

        settle_run(&mut world, "run-a".into(), TurnOutcome::Stopped(None));
        assert_eq!(state(&store, "run-a"), RunState::Cancelled);
        assert_eq!(agent_of(&mut world, "w1"), Some(kept));

        close(&mut world, "w1").unwrap();
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Closed
        );
        assert!(agent_of(&mut world, "w1").is_none());
    }
}
