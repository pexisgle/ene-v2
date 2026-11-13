//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` is the context that survives several runs; a `Run` is one
//! instruction turn. They are never conflated, and each run is bound to the
//! client generation captured when the work was commissioned.
//!
//! The session owns one long-lived work agent (rig) or one ACP process (see
//! [`acp`]). A run rebinds that agent to its run id, so `report`, permission
//! requests and turn settlement always see the active run. When a run ends the
//! agent survives; only a closed session, a failed start, or an explicitly
//! retired session releases it.
pub(crate) mod acp;
pub(crate) mod continuity;
pub(crate) mod sandbox;
pub(crate) mod tools;

use self::acp::{AcpBackends, AcpEvents};
use super::{
    agent::{self, WorkReports},
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

/// How long a run may stay silent before the parent is told once.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a stop may take before the process group is forced down.
pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the forced kill may take before the stop is unverified.
const FORCE_GRACE: Duration = Duration::from_secs(5);
/// The interval between streamed ACP updates forwarded to the parent.
const ACP_UPDATE_INTERVAL: Duration = Duration::from_secs(2);

/// The public state of a session, derived from its closed flag and active run.
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
        Some(match text {
            "idle" => Self::Idle,
            "active" => Self::Active,
            "closed" => Self::Closed,
            _ => return None,
        })
    }
}

/// The state of one run. Detail fields live next to it in the row so a run
/// always reads back exactly as it was written.
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
        Some(match text {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "waiting_input" => Self::WaitingInput,
            "cancelling" => Self::Cancelling,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "cancellation_unconfirmed" => Self::CancellationUnconfirmed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::CancellationUnconfirmed | Self::Failed
        )
    }

    /// Whether a run in this state can still produce events.
    pub(crate) fn is_open(self) -> bool {
        !self.is_terminal()
    }
}

/// The triggers of the run state machine (docs/design/work.md §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEvent {
    /// The backend accepted the run.
    Start,
    /// A stop was requested; the run is cancelling.
    Stop,
    /// The run finished normally.
    Complete,
    /// The backend confirmed a cancellation.
    CancelConfirmed,
    /// The forced stop could not be verified.
    CancelUnconfirmed,
    /// The run failed.
    Fail,
    /// A pending inquiry was answered.
    Resume,
}

/// The documented run transition, or `None` when the event cannot apply.
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
    /// The detail fields the contract carries for a run in this state.
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

/// A question the run asked the user, distinct from an approval request.
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

// === Live run bookkeeping ===============================================

/// Per-run liveness, kept out of SQLite because it is sub-minute data.
#[derive(Resource, Default)]
pub(crate) struct RunActivity {
    runs: HashMap<String, Activity>,
}

#[derive(Clone, Debug)]
struct Activity {
    work_id: String,
    last: Instant,
    last_forwarded: Instant,
    warned: bool,
}

impl RunActivity {
    fn begin(&mut self, work_id: &str, run_id: &str) {
        let now = Instant::now();
        self.runs.insert(
            run_id.to_owned(),
            Activity {
                work_id: work_id.to_owned(),
                last: now,
                last_forwarded: now,
                warned: false,
            },
        );
    }

    fn touch(&mut self, work_id: &str, run_id: &str) {
        match self.runs.get_mut(run_id) {
            Some(activity) => {
                activity.last = Instant::now();
                activity.warned = false;
            }
            None => self.begin(work_id, run_id),
        }
    }

    fn forward_due(&mut self, run_id: &str, interval: Duration) -> bool {
        let Some(activity) = self.runs.get_mut(run_id) else {
            return false;
        };
        let now = Instant::now();
        if now.saturating_duration_since(activity.last_forwarded) < interval {
            return false;
        }
        activity.last_forwarded = now;
        true
    }

    fn forget(&mut self, run_id: &str) {
        self.runs.remove(run_id);
    }

    fn stalled(&mut self, now: Instant, threshold: Duration) -> Vec<(String, String)> {
        let mut due = Vec::new();
        for (run_id, activity) in &mut self.runs {
            if due_for_stall(activity, now, threshold) {
                due.push((run_id.clone(), activity.work_id.clone()));
            }
        }
        due
    }
}

fn due_for_stall(activity: &mut Activity, now: Instant, threshold: Duration) -> bool {
    if activity.warned || now.saturating_duration_since(activity.last) < threshold {
        return false;
    }
    activity.warned = true;
    true
}

/// Runs waiting for a cooperative stop or for the forced kill timeout.
#[derive(Resource, Default)]
pub(crate) struct PendingStops {
    stops: HashMap<String, PendingStop>,
}

#[derive(Clone, Debug)]
struct PendingStop {
    work_id: String,
    reason: String,
    deadline: Instant,
    force_sent: bool,
}

impl PendingStops {
    fn arm(&mut self, run_id: &str, work_id: &str, reason: &str) {
        self.stops.insert(
            run_id.to_owned(),
            PendingStop {
                work_id: work_id.to_owned(),
                reason: reason.to_owned(),
                deadline: Instant::now() + STOP_TIMEOUT,
                force_sent: false,
            },
        );
    }

    fn take_due(&mut self, now: Instant) -> Vec<(String, PendingStop)> {
        let due: Vec<String> = self
            .stops
            .iter()
            .filter(|(_, stop)| stop.deadline <= now)
            .map(|(run_id, _)| run_id.clone())
            .collect();
        due.into_iter()
            .filter_map(|run_id| self.stops.remove(&run_id).map(|stop| (run_id, stop)))
            .collect()
    }

    fn reinsert(&mut self, run_id: String, stop: PendingStop) {
        self.stops.insert(run_id, stop);
    }

    fn remove(&mut self, run_id: &str) {
        self.stops.remove(run_id);
    }
}

/// How a stop was confirmed, for classification into `Cancelled` /
/// `CancellationUnconfirmed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopConfirmation {
    /// The backend reported a cooperative stop.
    Cooperative,
    /// The process group was killed and its exit was observed.
    ForcedConfirmed,
    /// The process could not be verified as gone.
    ForcedUnconfirmed,
}

pub(crate) fn classify_stop(confirmation: StopConfirmation) -> RunState {
    match confirmation {
        StopConfirmation::Cooperative | StopConfirmation::ForcedConfirmed => RunState::Cancelled,
        StopConfirmation::ForcedUnconfirmed => RunState::CancellationUnconfirmed,
    }
}

/// Idempotent `SendWork` receipts outside the rig conversation log.
///
/// The rig backend keeps them in the session log aside (the canonical place),
/// but an ACP session has no rig conversation to store them in, so the same
/// receipt is also kept here for the lifetime of the host. They are not
/// persisted; a restart forgets them (documented approximation).
#[derive(Resource, Default)]
pub(crate) struct Receipts {
    records: HashMap<String, StoredReceipt>,
}

#[derive(Clone, Debug)]
struct StoredReceipt {
    hash: Vec<u8>,
    request_id: String,
}

impl Receipts {
    fn get(&self, work_id: &str, request_key: &str) -> Option<StoredReceipt> {
        self.records.get(&key(work_id, request_key)).cloned()
    }

    fn put(&mut self, work_id: &str, request_key: &str, hash: &[u8], request_id: &str) {
        self.records.insert(
            key(work_id, request_key),
            StoredReceipt {
                hash: hash.to_vec(),
                request_id: request_id.to_owned(),
            },
        );
    }
}

fn key(work_id: &str, request_key: &str) -> String {
    format!("{work_id}:{request_key}")
}

// === Host operations ======================================================

/// Registers the work tools and the turn-settle path.
pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
        app.init_resource::<RunActivity>()
            .init_resource::<PendingStops>()
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

/// Reconciles the long-lived session agents after a restart and restarts the
/// stall clock of every run that is still open.
pub(crate) fn restore(world: &mut World) {
    continuity::restore(world);
    let store = world.resource::<Store>().clone();
    let mut open = Vec::new();
    for companion in store.companions().unwrap_or_default() {
        for session in store.work_sessions(&companion.id).unwrap_or_default() {
            if let Ok(Some(run)) = store.active_run(&session.id) {
                open.push((session.id, run.run_id));
            }
        }
    }
    let mut activity = world.resource_mut::<RunActivity>();
    for (work_id, run_id) in open {
        activity.begin(&work_id, &run_id);
    }
}

pub(crate) fn list(store: &Store, companion: &str) -> Result<w::WorkViews> {
    store
        .companion(companion)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
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
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    let runs = store.runs(work_id)?;
    let inquiries = store.pending_inquiries(work_id)?;
    Ok(wire::work_view(&session, &runs, &inquiries))
}

/// Commissions work from a client. The client generation is captured now and
/// the hand's device operations are bound to it.
pub(crate) fn start(world: &mut World, request: w::StartWorkRequest) -> Result<w::WorkSession> {
    let store = world.resource::<Store>().clone();
    let companion = store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    if request.instruction.trim().is_empty() {
        return Err(HostError::invalid("an instruction is required"));
    }
    let (client_id, activation_revision) = binding_for(&store, &request)?;
    let work_id = uuid::Uuid::new_v4().to_string();
    let session = WorkSession {
        id: work_id.clone(),
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
        let _ = store.set_work_status(&work_id, WorkStatus::Closed);
        continuity::retire(world, &work_id);
        acp::shutdown(world, &work_id);
        return Err(error);
    }
    let session = store
        .work_session(&work_id)?
        .ok_or_else(|| HostError::storage("work session insert did not persist"))?;
    Ok(wire::work_session(&session))
}

/// The backend a new run of this session must use.
enum PlannedBackend {
    Rig,
    /// An ACP process must be started for this session.
    Acp(AcpConnection),
    /// The ACP process is already running.
    AcpLive,
}

fn plan_backend(
    world: &mut World,
    store: &Store,
    companion: &Companion,
    work_id: &str,
) -> Result<PlannedBackend> {
    if world.resource::<AcpBackends>().is_acp(work_id) {
        return Ok(PlannedBackend::AcpLive);
    }
    if continuity::agent_of(world, work_id).is_some() {
        return Ok(PlannedBackend::Rig);
    }
    let Some(assignment) = companion.model(Role::Task) else {
        return Ok(PlannedBackend::Rig);
    };
    let Some(connection) = store.connection(&assignment.connection_id)? else {
        return Ok(PlannedBackend::Rig);
    };
    match connection.config {
        ConnectionConfig::Acp(config) => {
            if !connection.enabled {
                return Err(HostError::unavailable(
                    "the assigned ACP connection is disabled",
                ));
            }
            if config.command.trim().is_empty() {
                return Err(HostError::unavailable("the ACP connection has no command"));
            }
            // A session that already ran on ACP keeps its context in that
            // process. Starting a fresh one would silently drop it.
            if !store.runs(work_id)?.is_empty() {
                return Err(HostError::unavailable(
                    "the ACP agent of this work session is no longer running",
                ));
            }
            Ok(PlannedBackend::Acp(config))
        }
        _ => Ok(PlannedBackend::Rig),
    }
}

/// Starts one run in an existing session, reusing its agent or ACP process.
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
    // One open run per session: the next instruction is a new run only after
    // the previous one settles.
    if let Some(active) = store.active_run(&session.id)? {
        return Err(HostError::conflict(format!(
            "the work session is already running run {}",
            active.run_id
        )));
    }
    let companion = store
        .companion(&session.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let planned = plan_backend(world, &store, &companion, &session.id)?;
    let starting = matches!(planned, PlannedBackend::Acp(_));
    let run_id = uuid::Uuid::new_v4().to_string();
    let run = Run {
        run_id: run_id.clone(),
        work_id: session.id.clone(),
        instruction: instruction.to_owned(),
        state: if starting {
            RunState::Starting
        } else {
            RunState::Running
        },
        pending_request_id: None,
        failure: None,
        cancellation_requested_at: None,
        cancellation_reason: None,
        unverified_pids: Vec::new(),
        started_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_run(&run)?;
    store.set_work_status(&session.id, WorkStatus::Active)?;
    let failure = match planned {
        PlannedBackend::Rig => {
            let agent = continuity::ensure(world, &companion, &session.id, &run_id, instruction);
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
        PlannedBackend::AcpLive => {
            let prompt = {
                let backends = world.resource::<AcpBackends>();
                backends
                    .handle(&session.id)
                    .is_some_and(|handle| handle.prompt(&run_id, instruction))
            };
            if prompt {
                None
            } else {
                Some(HostError::unavailable(
                    "the ACP agent is not accepting prompts",
                ))
            }
        }
        PlannedBackend::Acp(connection) => match acp::spawn(world, &session.id, &connection) {
            Ok(handle) => {
                let prompt = handle.prompt(&run_id, instruction);
                world.resource_mut::<AcpBackends>().insert(handle);
                if prompt {
                    None
                } else {
                    acp::shutdown(world, &session.id);
                    Some(HostError::unavailable(
                        "the ACP agent is not accepting prompts",
                    ))
                }
            }
            Err(error) => Some(error),
        },
    };
    if let Some(error) = failure {
        let _ = fail_run(&store, &run, &error);
        let _ = store.set_work_status(&session.id, WorkStatus::Idle);
        if let Ok(Some(session)) = store.work_session(&session.id) {
            notify_status(world, &session);
            if let Ok(Some(failed)) = store.run(&run_id) {
                notify_run(
                    world,
                    &session,
                    &failed,
                    w::WorkEventKind::RunFinished,
                    w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(&failed))),
                );
            }
        }
        return Err(error);
    }
    world
        .resource_mut::<RunActivity>()
        .begin(&session.id, &run_id);
    let session = store
        .work_session(&session.id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
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
    if !request.client_id.is_empty() {
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
        return Ok((activation.client_id, activation.revision));
    }
    Ok(activation
        .map(|activation| (activation.client_id, activation.revision))
        .unwrap_or((None, 0)))
}

/// Adds text to the active run of a session, or starts a new run when the
/// session is idle. `request_id` binds the text to a pending inquiry;
/// `request_key` makes a retry return the original acceptance.
pub(crate) fn send(world: &mut World, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(&request.work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    let inquiry_id = request.request_id.clone().unwrap_or_default();
    let hash = super::db::digest(
        format!("{}:{}:{}", request.work_id, request.text, inquiry_id).as_bytes(),
    );
    let agent = continuity::agent_of(world, &request.work_id);
    if !request.request_key.is_empty()
        && let Some(receipt) = receipt(world, agent, &request.work_id, &request.request_key)
    {
        if receipt.hash != hash {
            return Err(HostError::already_exists(
                "this request key already accepted different content",
            ));
        }
        return Ok(w::WorkAcceptance {
            request_id: receipt.request_id,
            ..Default::default()
        });
    }
    let inquiry = if inquiry_id.is_empty() {
        None
    } else {
        let inquiry = store
            .inquiry(&inquiry_id)?
            .ok_or_else(|| HostError::not_found("inquiry not found"))?;
        if inquiry.work_id != request.work_id {
            return Err(HostError::invalid(
                "the inquiry belongs to another work session",
            ));
        }
        Some(inquiry)
    };
    let acceptance = match store.active_run(&request.work_id)? {
        Some(mut run) => {
            if run.state.is_terminal() {
                return Err(HostError::conflict("work session has no active run"));
            }
            if world.resource::<AcpBackends>().is_acp(&request.work_id) {
                return Err(HostError::conflict(
                    "the ACP agent is running a prompt; wait for it to finish before sending more input",
                ));
            }
            let Some(agent) = agent else {
                return Err(HostError::unavailable("the work agent is not running"));
            };
            continuity::bind(world, agent, &run.run_id);
            if let Some(inquiry) = &inquiry {
                if inquiry.run_id != run.run_id {
                    return Err(HostError::conflict(
                        "the inquiry belongs to a run that is no longer active",
                    ));
                }
                store.resolve_inquiry(&inquiry_id, &request.text)?;
                run.state = transition(run.state, RunEvent::Resume).unwrap_or(RunState::Running);
                run.pending_request_id = None;
                store.update_run(&run)?;
                notify_run(
                    world,
                    &session,
                    &run,
                    w::WorkEventKind::StatusChanged,
                    w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
                        status: w::WorkStatus::Active.into(),
                        ..Default::default()
                    })),
                );
            }
            world.trigger(Deliver::new(
                agent,
                request.text.clone(),
                DeliveryMode::Steer,
            ));
            world
                .resource_mut::<RunActivity>()
                .touch(&request.work_id, &run.run_id);
            if inquiry_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                inquiry_id.clone()
            }
        }
        None => {
            let run = start_run(world, &session, &request.text)?;
            if let Some(inquiry) = &inquiry {
                store.resolve_inquiry(&inquiry.request_id, &request.text)?;
            }
            world
                .resource_mut::<RunActivity>()
                .touch(&request.work_id, &run.run_id);
            if inquiry_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                inquiry_id.clone()
            }
        }
    };
    if !request.request_key.is_empty() {
        let agent = continuity::agent_of(world, &request.work_id);
        record_receipt(
            world,
            agent,
            &request.work_id,
            &request.request_key,
            &hash,
            &acceptance,
        );
    }
    if let Ok(Some(session)) = store.work_session(&request.work_id) {
        notify_status(world, &session);
    }
    Ok(w::WorkAcceptance {
        request_id: acceptance,
        ..Default::default()
    })
}

fn receipt(
    world: &World,
    agent: Option<Entity>,
    work_id: &str,
    request_key: &str,
) -> Option<StoredReceipt> {
    if let Some(agent) = agent
        && let Some(conversation) = world.get::<Conversation>(agent)
    {
        for (_, aside) in conversation
            .asides()
            .filter(|(_, aside)| aside.kind == "ene.work_accepted")
        {
            if aside
                .value
                .get("request_key")
                .and_then(|value| value.as_str())
                != Some(request_key)
            {
                continue;
            }
            let hash: Vec<u8> = aside
                .value
                .get("hash")
                .and_then(|hash| hash.as_array())
                .map(|bytes| {
                    bytes
                        .iter()
                        .filter_map(serde_json::Value::as_u64)
                        .map(|byte| byte as u8)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(request_id) = aside.value.get("request_id").and_then(|id| id.as_str()) {
                return Some(StoredReceipt {
                    hash,
                    request_id: request_id.to_owned(),
                });
            }
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

/// Answers one pending inquiry from the UI.
pub(crate) fn answer_inquiry(
    world: &mut World,
    request: w::AnswerInquiryRequest,
) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let inquiry = store
        .inquiry(&request.request_id)?
        .ok_or_else(|| HostError::not_found("inquiry not found"))?;
    if inquiry.work_id != request.work_id {
        return Err(HostError::invalid(
            "the inquiry belongs to another work session",
        ));
    }
    let session = store
        .work_session(&request.work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    if world.resource::<AcpBackends>().is_acp(&request.work_id) {
        return Err(HostError::unavailable(
            "ACP work sessions do not report inquiries",
        ));
    }
    let run = store
        .active_run(&request.work_id)?
        .ok_or_else(|| HostError::conflict("work session has no active run"))?;
    if run.run_id != inquiry.run_id {
        return Err(HostError::conflict(
            "the inquiry belongs to a run that is no longer active",
        ));
    }
    let agent = continuity::agent_of(world, &request.work_id)
        .ok_or_else(|| HostError::unavailable("the work agent is not running"))?;
    continuity::bind(world, agent, &run.run_id);
    store.resolve_inquiry(&request.request_id, &request.answer)?;
    let mut answer = run.clone();
    answer.state = transition(answer.state, RunEvent::Resume).unwrap_or(RunState::Running);
    answer.pending_request_id = None;
    store.update_run(&answer)?;
    world
        .resource_mut::<RunActivity>()
        .touch(&request.work_id, &answer.run_id);
    world.trigger(Deliver::new(
        agent,
        format!("ユーザーからの回答: {}", request.answer),
        DeliveryMode::Steer,
    ));
    notify_run(
        world,
        &session,
        &answer,
        w::WorkEventKind::StatusChanged,
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: w::WorkStatus::Active.into(),
            ..Default::default()
        })),
    );
    Ok(w::WorkAcceptance {
        request_id: request.request_id,
        ..Default::default()
    })
}

/// Requests a cooperative stop. The turn-settle observer records the outcome;
/// [`expire_stops`] forces the process group down after [`STOP_TIMEOUT`].
pub(crate) fn stop(world: &mut World, work_id: &str) -> Result<()> {
    stop_with(world, work_id, None)
}

/// The same stop with the user's reason recorded on the run.
pub(crate) fn stop_with(world: &mut World, work_id: &str, reason: Option<&str>) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
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
        && let Some(agent) = continuity::agent_of(world, work_id)
    {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some(reason.clone()),
        });
    }
    if !already_cancelling {
        world
            .resource_mut::<PendingStops>()
            .arm(&run.run_id, work_id, &reason);
    }
    Ok(())
}

/// Closes a session. Any open run is stopped and the agent is released; an ACP
/// process is terminated with the session.
pub(crate) fn close(world: &mut World, work_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
        return Ok(());
    }
    stop_with(world, work_id, Some("the work session is closing"))?;
    store.close_work(work_id)?;
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    notify_status(world, &session);
    continuity::retire(world, work_id);
    acp::shutdown(world, work_id);
    Ok(())
}

/// Called when a work agent's turn settled: records the run, reports to the
/// parent and keeps the session agent alive.
pub(crate) fn settle_run(world: &mut World, run_id: String, outcome: TurnOutcome) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.run(&run_id) else {
        return;
    };
    if run.state.is_terminal() {
        return;
    }
    let reported_failure = world.resource::<WorkReports>().failed.get(&run_id).cloned();
    let (state, text) = match &outcome {
        TurnOutcome::Answered(message) => {
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
        TurnOutcome::Stopped(_) => (classify_stop(StopConfirmation::Cooperative), None),
        TurnOutcome::Failed(why) => (
            transition(run.state, RunEvent::Fail).unwrap_or(RunState::Failed),
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    };
    let (state, text) = match reported_failure {
        Some(reason) => (RunState::Failed, Some(reason)),
        None => (state, text),
    };
    finish_run(world, run, state, text, None);
}

/// Records a forced stop, classifying it as `Cancelled` only when the process
/// is known to be gone.
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
        world.resource_mut::<PendingStops>().remove(run_id);
        return;
    }
    let confirmation = if confirmed {
        StopConfirmation::ForcedConfirmed
    } else {
        StopConfirmation::ForcedUnconfirmed
    };
    let event = if confirmed {
        RunEvent::CancelConfirmed
    } else {
        RunEvent::CancelUnconfirmed
    };
    let state = transition(run.state, event).unwrap_or_else(|| classify_stop(confirmation));
    let text = (state == RunState::CancellationUnconfirmed).then(|| {
        format!("作業を停止しましたが、完全に停止したか確認できませんでした（{reason}）。")
    });
    finish_run(world, run, state, text, Some((reason.to_owned(), pids)));
}

/// Marks a run as running once its backend is ready.
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
    let was_active = store
        .active_run(&run.work_id)
        .ok()
        .flatten()
        .is_some_and(|active| active.run_id == run.run_id);
    run.state = state;
    run.pending_request_id = None;
    match state {
        RunState::Failed => {
            run.failure = Some(Failure::new(
                "work_failed",
                text.clone().unwrap_or_else(|| "the run failed".to_owned()),
            ));
        }
        RunState::Cancelled => {
            run.unverified_pids.clear();
        }
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
    if was_active {
        if let Some(session) = &session {
            if session.status != WorkStatus::Closed {
                let _ = store.set_work_status(&session.id, WorkStatus::Idle);
            }
            if let Ok(Some(session)) = store.work_session(&session.id) {
                notify_status(world, &session);
                notify_run(
                    world,
                    &session,
                    &run,
                    w::WorkEventKind::RunFinished,
                    w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(&run))),
                );
                if let Some(parent) = agent::agent_of(world, &session.companion_id) {
                    let report = text.unwrap_or_else(|| match state {
                        RunState::Cancelled => "作業を中断しました".to_owned(),
                        _ => "作業が終わりました".to_owned(),
                    });
                    let author = Author::agent(
                        agent::work_agent_id(&run.run_id),
                        Some(RequestId(run.run_id.clone())),
                    );
                    world.trigger(
                        Deliver::new(parent, report, DeliveryMode::Queue).with_author(author),
                    );
                }
            }
        }
    } else if let Some(session) = &session {
        // A late event of an older run is still an accurate audit record of
        // that run, but it must never change the new run's session state.
        notify_run(
            world,
            session,
            &run,
            w::WorkEventKind::RunFinished,
            w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(&run))),
        );
    }
    let closed = session
        .as_ref()
        .is_some_and(|session| session.status == WorkStatus::Closed);
    if closed {
        continuity::retire(world, &run.work_id);
        acp::shutdown(world, &run.work_id);
    }
    world.resource_mut::<RunActivity>().forget(&run.run_id);
    world.resource_mut::<PendingStops>().remove(&run.run_id);
    world
        .resource_mut::<WorkReports>()
        .completed
        .remove(&run.run_id);
    world
        .resource_mut::<WorkReports>()
        .failed
        .remove(&run.run_id);
}

/// Forwards one streamed ACP update, throttled so the parent's transcript is
/// not flooded by token-sized chunks.
pub(crate) fn acp_update(world: &mut World, work_id: &str, run_id: &str, text: &str) {
    let forward = {
        let mut activity = world.resource_mut::<RunActivity>();
        activity.touch(work_id, run_id);
        activity.forward_due(run_id, ACP_UPDATE_INTERVAL)
    };
    if !forward {
        return;
    }
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let Some(parent) = parent_of(world, work_id) else {
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

/// The ACP connection ended. A cancelling run is classified from the process
/// result; any other open run fails honestly instead of looking completed.
pub(crate) fn acp_closed(
    world: &mut World,
    work_id: &str,
    run_id: Option<String>,
    error: Option<String>,
    confirmed: bool,
    pids: Vec<u32>,
) {
    world.resource_mut::<AcpBackends>().remove(work_id);
    let Some(run_id) = run_id else {
        return;
    };
    let store = world.resource::<Store>().clone();
    let Ok(Some(run)) = store.run(&run_id) else {
        return;
    };
    if run.state.is_terminal() {
        world.resource_mut::<PendingStops>().remove(&run_id);
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

/// Shows a sandbox notice for the session; never claims isolation.
pub(crate) fn acp_warning(world: &mut World, work_id: &str, message: &str) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(session)) = store.work_session(work_id) else {
        return;
    };
    let message = super::db::truncate(message, 500);
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(
        Some(&session.companion_id),
        sandbox::NON_ISOLATED_CODE,
        &message,
    );
}

/// Records progress from a hand so the stall timer restarts.
pub(crate) fn note_activity(world: &mut World, work_id: &str, run_id: &str) {
    world.resource_mut::<RunActivity>().touch(work_id, run_id);
}

/// Injects one notice into the parent when a running run went silent for
/// [`STALL_TIMEOUT`]. Exactly once per silence, as `stall_warned` records.
fn detect_stalls(world: &mut World) {
    let now = Instant::now();
    let due = world
        .resource_mut::<RunActivity>()
        .stalled(now, STALL_TIMEOUT);
    for (run_id, work_id) in due {
        let store = world.resource::<Store>().clone();
        let Ok(Some(run)) = store.run(&run_id) else {
            continue;
        };
        if run.state != RunState::Running {
            continue;
        }
        let Ok(Some(session)) = store.work_session(&work_id) else {
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

/// Enforces the 30 second stop timeout without blocking the ECS thread.
fn expire_stops(world: &mut World) {
    let now = Instant::now();
    let due = world.resource_mut::<PendingStops>().take_due(now);
    for (run_id, mut stop) in due {
        let store = world.resource::<Store>().clone();
        let Ok(Some(run)) = store.run(&run_id) else {
            continue;
        };
        if !run.state.is_open() {
            continue;
        }
        if stop.force_sent {
            let pids = acp::pids(world, &stop.work_id);
            let reason = format!("{} (the process group did not exit)", stop.reason);
            record_forced_stop(world, &run_id, false, pids, &reason);
            continue;
        }
        if !acp::force_kill(world, &stop.work_id) {
            // The rig backend runs in-process: there is no process group to
            // kill, and a turn that ignored `StopTurn` for 30 seconds is not
            // verifiably stopped.
            let reason = format!("{} (no process group to kill)", stop.reason);
            record_forced_stop(world, &run_id, false, Vec::new(), &reason);
            continue;
        }
        stop.force_sent = true;
        stop.deadline = now + FORCE_GRACE;
        world.resource_mut::<PendingStops>().reinsert(run_id, stop);
    }
}

fn parent_of(world: &mut World, work_id: &str) -> Option<Entity> {
    let store = world.resource::<Store>().clone();
    let session = store.work_session(work_id).ok().flatten()?;
    agent::agent_of(world, &session.companion_id)
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

fn notify_status(world: &mut World, session: &WorkSession) {
    let event = wire::work_event(
        session,
        w::WorkEventKind::StatusChanged,
        w::work_event::Detail::StatusChanged(Box::new(w::WorkStatusChanged {
            status: wire::work_status(session.status).into(),
            ..Default::default()
        })),
    );
    let companion = session.companion_id.clone();
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        Some(&companion),
        None,
        w::notification::Event::WorkEvent(Box::new(event)),
    );
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
    let companion = session.companion_id.clone();
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        Some(&companion),
        None,
        w::notification::Event::WorkEvent(Box::new(event)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        agent::WorkAgent,
        json::JsonDoc,
        mind::{Companion, Profile, State},
        notify::Notifier,
        work::continuity::SessionAgent,
    };

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        (root, store)
    }

    fn session(store: &Store, id: &str) -> WorkSession {
        let session = WorkSession {
            id: id.into(),
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

    fn insert_run(store: &Store, work_id: &str, run_id: &str, state: RunState) {
        store
            .insert_run(&Run {
                run_id: run_id.into(),
                work_id: work_id.into(),
                instruction: "do it".into(),
                state,
                pending_request_id: None,
                failure: None,
                cancellation_requested_at: None,
                cancellation_reason: None,
                unverified_pids: Vec::new(),
                started_at: format!("2026-01-01T00:00:00Z-{run_id}"),
                updated_at: String::new(),
            })
            .unwrap();
    }

    fn world(store: Store) -> World {
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(Notifier::new());
        world.insert_resource(WorkReports::default());
        world.insert_resource(RunActivity::default());
        world.insert_resource(PendingStops::default());
        world.insert_resource(Receipts::default());
        world.insert_resource(acp::AcpBackends::default());
        world.insert_resource(acp::AcpEvents::default());
        world
    }

    fn companion() -> Companion {
        Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Default::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn run_and_session_status_round_trip() {
        for state in [
            RunState::Starting,
            RunState::Running,
            RunState::WaitingInput,
            RunState::Cancelling,
            RunState::Completed,
            RunState::Cancelled,
            RunState::CancellationUnconfirmed,
            RunState::Failed,
        ] {
            assert_eq!(RunState::parse(state.as_str()), Some(state));
            assert_eq!(state.is_open(), !state.is_terminal());
        }
        assert!(RunState::Cancelled.is_terminal());
        assert!(!RunState::Cancelling.is_terminal());
        for status in [WorkStatus::Idle, WorkStatus::Active, WorkStatus::Closed] {
            assert_eq!(WorkStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn the_run_state_machine_follows_the_contract() {
        use RunEvent::*;
        use RunState::*;
        assert_eq!(transition(Starting, Start), Some(Running));
        assert_eq!(transition(Running, Stop), Some(Cancelling));
        assert_eq!(transition(WaitingInput, Stop), Some(Cancelling));
        assert_eq!(transition(WaitingInput, Resume), Some(Running));
        assert_eq!(transition(Running, Complete), Some(Completed));
        assert_eq!(transition(Cancelling, CancelConfirmed), Some(Cancelled));
        assert_eq!(
            transition(Cancelling, CancelUnconfirmed),
            Some(CancellationUnconfirmed)
        );
        assert_eq!(transition(Running, Fail), Some(Failed));
        for terminal in [Completed, Cancelled, CancellationUnconfirmed, Failed] {
            assert_eq!(transition(terminal, Stop), None);
            assert_eq!(transition(terminal, Complete), None);
        }
    }

    #[test]
    fn a_session_agent_survives_a_finished_run() {
        let (_root, store) = store();
        let session = session(&store, "w1");
        insert_run(&store, "w1", "run-a", RunState::Running);
        let mut world = world(store.clone());
        let entity = world
            .spawn((
                WorkAgent {
                    work_id: "w1".into(),
                    run_id: "run-a".into(),
                    companion_id: "ene".into(),
                },
                SessionAgent {
                    work_id: "w1".into(),
                },
            ))
            .id();
        settle_run(&mut world, "run-a".into(), TurnOutcome::Stopped(None));
        let stored = store.run("run-a").unwrap().unwrap();
        assert_eq!(stored.state, RunState::Cancelled);
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Idle,
            "a finished run leaves the session idle, not closed"
        );
        assert!(
            world.get::<WorkAgent>(entity).is_some(),
            "the session agent survives the run"
        );
        assert_eq!(continuity::agent_of(&mut world, "w1"), Some(entity));

        // The next run reuses that agent instead of spawning another one.
        insert_run(&store, "w1", "run-b", RunState::Running);
        continuity::bind(&mut world, entity, "run-b");
        assert_eq!(continuity::agent_of(&mut world, "w1"), Some(entity));
        assert!(agent::work_agent_of(&mut world, "run-b").is_some());
        assert!(agent::work_agent_of(&mut world, "run-a").is_none());
        let _ = session;
    }

    #[test]
    fn a_new_run_reuses_the_one_session_agent() {
        let mut world = World::new();
        let companion = companion();
        let first = continuity::ensure(&mut world, &companion, "w1", "run-a", "list the folder");
        let second = continuity::ensure(&mut world, &companion, "w1", "run-b", "summarize it");
        assert_eq!(
            first, second,
            "the second run binds the existing agent instead of spawning one"
        );
        assert_eq!(
            world
                .get::<WorkAgent>(first)
                .map(|agent| agent.run_id.clone()),
            Some("run-b".to_owned())
        );
        let count = {
            let mut query = world.query::<&WorkAgent>();
            query
                .iter(&world)
                .filter(|agent| agent.work_id == "w1")
                .count()
        };
        assert_eq!(count, 1, "a session has exactly one work agent");
    }

    #[test]
    fn a_late_run_does_not_overwrite_the_new_run() {
        let (_root, store) = store();
        session(&store, "w1");
        insert_run(&store, "w1", "run-a", RunState::Running);
        insert_run(&store, "w1", "run-b", RunState::Running);
        let mut world = world(store.clone());
        // Run B is the active one; a settlement for run A arrives late.
        settle_run(&mut world, "run-a".into(), TurnOutcome::Stopped(None));
        assert_eq!(
            store.work_session("w1").unwrap().unwrap().status,
            WorkStatus::Active,
            "the late run's finish does not idle the session"
        );
        assert_eq!(
            store.run("run-a").unwrap().unwrap().state,
            RunState::Cancelled
        );
        assert_eq!(
            store.run("run-b").unwrap().unwrap().state,
            RunState::Running
        );
    }

    #[test]
    fn a_stall_is_warned_once_and_reset_by_activity() {
        let now = Instant::now();
        let mut activity = Activity {
            work_id: "w1".into(),
            last: now - STALL_TIMEOUT - Duration::from_secs(1),
            last_forwarded: now,
            warned: false,
        };
        assert!(due_for_stall(&mut activity, now, STALL_TIMEOUT));
        assert!(
            !due_for_stall(&mut activity, now, STALL_TIMEOUT),
            "stall_warned keeps the notice to one"
        );
        activity.last = now;
        activity.warned = false;
        assert!(!due_for_stall(&mut activity, now, STALL_TIMEOUT));
        assert!(due_for_stall(
            &mut activity,
            now + STALL_TIMEOUT + Duration::from_secs(1),
            STALL_TIMEOUT
        ));
    }

    #[test]
    fn a_cancellation_timeout_is_classified_honestly() {
        assert_eq!(
            classify_stop(StopConfirmation::Cooperative),
            RunState::Cancelled
        );
        assert_eq!(
            classify_stop(StopConfirmation::ForcedConfirmed),
            RunState::Cancelled
        );
        assert_eq!(
            classify_stop(StopConfirmation::ForcedUnconfirmed),
            RunState::CancellationUnconfirmed
        );

        let mut stops = PendingStops::default();
        stops.arm("run-a", "w1", "the user stopped the work");
        assert!(
            stops.take_due(Instant::now()).is_empty(),
            "the cooperative wait is still running"
        );
        let due = stops.take_due(Instant::now() + STOP_TIMEOUT + Duration::from_secs(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "run-a");
        assert_eq!(due[0].1.reason, "the user stopped the work");
        assert!(!due[0].1.force_sent);
    }

    #[test]
    fn an_unverified_forced_stop_records_the_unverified_pids() {
        let (_root, store) = store();
        session(&store, "w1");
        insert_run(&store, "w1", "run-a", RunState::Cancelling);
        let mut world = world(store.clone());
        record_forced_stop(
            &mut world,
            "run-a",
            false,
            vec![4242],
            "the process did not exit",
        );
        let stored = store.run("run-a").unwrap().unwrap();
        assert_eq!(stored.state, RunState::CancellationUnconfirmed);
        assert_eq!(stored.unverified_pids, vec![4242]);
        assert!(
            stored
                .cancellation_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("did not exit"))
        );

        insert_run(&store, "w1", "run-b", RunState::Cancelling);
        record_forced_stop(&mut world, "run-b", true, Vec::new(), "killed");
        assert_eq!(
            store.run("run-b").unwrap().unwrap().state,
            RunState::Cancelled
        );
    }
}
