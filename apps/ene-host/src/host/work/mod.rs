//! Work sessions and runs: the "hands" of docs/design/work.md.
//!
//! A `WorkSession` is the context that survives several runs; a `Run` is one
//! instruction turn. They are never conflated, and each run is bound to the
//! client generation captured when the work was commissioned.
pub(crate) mod tools;

use super::error::Failure;
use serde::{Deserialize, Serialize};

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

// === Host operations ======================================================

use super::{
    agent::{self, WorkReports},
    db::{Result, Store, now},
    error::HostError,
    notify::Notifier,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::Message;
use rig_ecs::prelude::*;
use serde_json::json;

/// Registers the work tools and the turn-settle path.
pub(crate) struct WorkPlugin;

impl Plugin for WorkPlugin {
    fn build(&self, app: &mut App) {
        tools::register(app);
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
    let run_id = uuid::Uuid::new_v4().to_string();
    let session = WorkSession {
        id: work_id.clone(),
        companion_id: companion.id.clone(),
        title: if request.title.trim().is_empty() {
            super::db::truncate(&request.instruction, 80)
        } else {
            request.title.clone()
        },
        status: WorkStatus::Active,
        client_id,
        activation_revision,
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_work_session(&session)?;
    let run = Run {
        run_id: run_id.clone(),
        work_id: work_id.clone(),
        instruction: request.instruction.clone(),
        state: RunState::Running,
        pending_request_id: None,
        failure: None,
        cancellation_requested_at: None,
        cancellation_reason: None,
        unverified_pids: Vec::new(),
        started_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_run(&run)?;
    let agent = agent::spawn_work_agent(world, &companion, &work_id, &run_id, &request.instruction);
    if let Err(error) = agent::ensure_model(world, agent) {
        store.set_work_status(&work_id, WorkStatus::Closed)?;
        let mut current = run.clone();
        current.state = RunState::Failed;
        current.failure = Some(super::error::Failure::from_error(&error));
        store.update_run(&current)?;
        if let Some(entity) = agent::work_agent_of(world, &run_id) {
            world.trigger(Retire { entity });
        }
        return Err(error);
    }
    world.trigger(Deliver::new(
        agent,
        request.instruction.clone(),
        DeliveryMode::Queue,
    ));
    let session = store
        .work_session(&work_id)?
        .ok_or_else(|| HostError::storage("work session insert did not persist"))?;
    notify_status(world, &session);
    notify_run(
        world,
        &session,
        &run,
        w::WorkEventKind::RunStarted,
        w::work_event::Detail::RunStarted(Box::default()),
    );
    Ok(wire::work_session(&session))
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

/// Adds text to the current run. `request_id` binds the text to a pending
/// inquiry; `request_key` makes a retry return the original acceptance.
pub(crate) fn send(world: &mut World, request: w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(&request.work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
        return Err(HostError::conflict("work session is closed"));
    }
    let run_id = active_run_id(&store, &request.work_id)?;
    let agent = agent::work_agent_of(world, &run_id)
        .ok_or_else(|| HostError::unavailable("the work agent is not running"))?;
    let inquiry_id = request.request_id.clone().unwrap_or_default();
    let hash = super::db::digest(
        format!("{}:{}:{}", request.work_id, request.text, inquiry_id).as_bytes(),
    );
    if !request.request_key.is_empty()
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
                != Some(request.request_key.as_str())
            {
                continue;
            }
            let recorded: Vec<u8> = aside
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
            if recorded != hash {
                return Err(HostError::already_exists(
                    "this request key already accepted different content",
                ));
            }
            if let Some(request_id) = aside.value.get("request_id").and_then(|id| id.as_str()) {
                return Ok(w::WorkAcceptance {
                    request_id: request_id.to_owned(),
                    ..Default::default()
                });
            }
        }
    }
    if !inquiry_id.is_empty() {
        let inquiry = store
            .inquiry(&inquiry_id)?
            .ok_or_else(|| HostError::not_found("inquiry not found"))?;
        if inquiry.work_id != request.work_id {
            return Err(HostError::invalid(
                "the inquiry belongs to another work session",
            ));
        }
        store.resolve_inquiry(&inquiry_id, &request.text)?;
        let mut run = store
            .run(&run_id)?
            .ok_or_else(|| HostError::not_found("run not found"))?;
        run.state = RunState::Running;
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
    let acceptance = if inquiry_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        inquiry_id
    };
    if !request.request_key.is_empty()
        && let Some(mut conversation) = world.get_mut::<Conversation>(agent)
    {
        conversation.add_aside(rig_core::conversation::Aside::new(
            "ene.work_accepted",
            rig_core::conversation::Author::plugin("ene-host"),
            json!({
                "request_key": request.request_key,
                "request_id": acceptance,
                "hash": hash,
                "accepted_at": now(),
            }),
        ));
    }
    notify_status(world, &session);
    Ok(w::WorkAcceptance {
        request_id: acceptance,
        ..Default::default()
    })
}

fn active_run_id(store: &Store, work_id: &str) -> Result<String> {
    store
        .active_run(work_id)?
        .map(|run| run.run_id)
        .ok_or_else(|| HostError::conflict("work session has no active run"))
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
    let agent = agent::work_agent_of(world, &inquiry.run_id)
        .ok_or_else(|| HostError::unavailable("the work agent is not running"))?;
    store.resolve_inquiry(&request.request_id, &request.answer)?;
    let mut run = store
        .run(&inquiry.run_id)?
        .ok_or_else(|| HostError::not_found("run not found"))?;
    run.state = RunState::Running;
    run.pending_request_id = None;
    store.update_run(&run)?;
    world.trigger(Deliver::new(
        agent,
        format!("ユーザーからの回答: {}", request.answer),
        DeliveryMode::Steer,
    ));
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
    Ok(w::WorkAcceptance {
        request_id: request.request_id,
        ..Default::default()
    })
}

/// Requests a cooperative stop. The turn-settle observer records the outcome.
pub(crate) fn stop(world: &mut World, work_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    if session.status == WorkStatus::Closed {
        return Ok(());
    }
    if let Some(run) = store.active_run(work_id)? {
        let mut run = run;
        run.state = RunState::Cancelling;
        run.cancellation_requested_at = Some(now());
        store.update_run(&run)?;
        if let Some(agent) = agent::work_agent_of(world, &run.run_id) {
            world.trigger(StopTurn {
                entity: agent,
                reason: Some("the user stopped the work".into()),
            });
        }
    }
    Ok(())
}

/// Closes a session. Any open run is stopped first.
pub(crate) fn close(world: &mut World, work_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    stop(world, work_id)?;
    store.close_work(work_id)?;
    let session = store
        .work_session(work_id)?
        .ok_or_else(|| HostError::not_found("work session not found"))?;
    notify_status(world, &session);
    if let Some(run_id) = store
        .active_run(work_id)
        .ok()
        .flatten()
        .map(|run| run.run_id)
        && let Some(entity) = agent::work_agent_of(world, &run_id)
    {
        world.trigger(Retire { entity });
    }
    Ok(())
}

/// Called when a work agent's turn settled: records the run, reports to the
/// parent and retires the agent.
pub(crate) fn settle_run(world: &mut World, run_id: String, outcome: TurnOutcome) {
    let store = world.resource::<Store>().clone();
    let Ok(Some(mut run)) = store.run(&run_id) else {
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
                RunState::Completed,
                report.or_else(|| message_text(message)),
            )
        }
        TurnOutcome::Stopped(_) => (RunState::Cancelled, None),
        TurnOutcome::Failed(why) => (
            RunState::Failed,
            Some(format!("作業を続けられませんでした: {why}")),
        ),
    };
    let (state, text) = match reported_failure {
        Some(reason) => (RunState::Failed, Some(reason)),
        None => (state, text),
    };
    run.state = state;
    run.pending_request_id = None;
    if state == RunState::Failed {
        run.failure = Some(super::error::Failure::new(
            "work_failed",
            outcome_text(&outcome),
        ));
    }
    if let Err(error) = store.update_run(&run) {
        super::fail(world, error.to_string());
        return;
    }
    let session = store.work_session(&run.work_id).ok().flatten();
    if let Some(session) = &session {
        let status = if session.status == WorkStatus::Closed {
            WorkStatus::Closed
        } else {
            WorkStatus::Idle
        };
        let _ = store.set_work_status(&session.id, status);
        if let Ok(Some(session)) = store.work_session(&session.id) {
            notify_status(world, &session);
        }
    }
    if let Some(session) = &session {
        notify_run(
            world,
            session,
            &run,
            w::WorkEventKind::RunFinished,
            w::work_event::Detail::RunFinished(Box::new(wire::work_run_finished(&run))),
        );
    }
    if let (Some(parent), Some(session)) = (parent_of(world, &run.work_id), session.as_ref()) {
        let report = text.unwrap_or_else(|| "作業が終わりました".into());
        let author = Author::agent(
            agent::work_agent_id(&run_id),
            Some(RequestId(run_id.clone())),
        );
        world.trigger(Deliver::new(parent, report, DeliveryMode::Queue).with_author(author));
        let _ = session;
    }
    if let Some(entity) = agent::work_agent_of(world, &run_id) {
        world.trigger(Retire { entity });
    }
    world
        .resource_mut::<WorkReports>()
        .completed
        .remove(&run_id);
    world.resource_mut::<WorkReports>().failed.remove(&run_id);
}

fn parent_of(world: &mut World, work_id: &str) -> Option<Entity> {
    let store = world.resource::<Store>().clone();
    let session = store.work_session(work_id).ok().flatten()?;
    agent::agent_of(world, &session.companion_id)
}

fn outcome_text(outcome: &TurnOutcome) -> String {
    match outcome {
        TurnOutcome::Failed(why) => why.clone(),
        TurnOutcome::Stopped(why) => why.clone().unwrap_or_else(|| "stopped".into()),
        TurnOutcome::Answered(message) => message_text(message).unwrap_or_default(),
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
        }
        assert!(RunState::Cancelled.is_terminal());
        assert!(!RunState::Cancelling.is_terminal());
        for status in [WorkStatus::Idle, WorkStatus::Active, WorkStatus::Closed] {
            assert_eq!(WorkStatus::parse(status.as_str()), Some(status));
        }
    }
}
