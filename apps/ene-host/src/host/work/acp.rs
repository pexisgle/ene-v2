//! The external ACP backend.
//!
//! One WorkSession owns one ACP process and one ACP session; a Run is one
//! `session/prompt` turn on that connection (docs/design/work.md §2.1 and
//! tech-stack.md §G). The process outlives a run so the agent keeps its
//! conversation context.
//!
//! ## Boundary with the ECS
//!
//! rig-ecs runs on the host's ECS thread and the ACP connection is Tokio-only,
//! so the two meet through two channels rather than by sharing state:
//!
//! * ECS -> backend: `tokio::sync::mpsc` commands (`prompt`, `cancel`,
//!   force-kill, shutdown) held by [`AcpHandle`] in the [`AcpBackends`]
//!   resource.
//! * backend -> ECS: a `crossbeam_channel` of [`AcpEvent`]s in [`AcpEvents`].
//!   [`pump`] drains it in `Update` after `AgentLoop` and calls the same
//!   `settle_run` path the rig backend uses, so a completion, failure or
//!   cancellation always lands in the run row once.
//!
//! The backend owns its own small Tokio runtime because the ECS thread has no
//! reactor and must never block on one.
use super::sandbox;
use crate::host::{error::HostError, settings::AcpConnection};
use agent_client_protocol::{
    Agent, ByteStreams, Client, on_receive_notification, on_receive_request,
    schema::ProtocolVersion,
    v1::{
        ConnectionTo,
        schema::{
            CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            SessionNotification, SessionUpdate, StopReason, TextContent,
        },
    },
};
use bevy_ecs::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rig_core::completion::Message;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncBufReadExt,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// How long the forced kill may take before it is reported as unverified.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// The terminal result of one ACP prompt turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcpOutcome {
    Completed,
    Cancelled,
    Failed,
}

/// One backend -> ECS message.
#[derive(Clone, Debug)]
pub(crate) enum AcpEvent {
    /// The ACP session exists; the first prompt can be served.
    Ready { run_id: String },
    /// A streamed agent update that belongs to the active run.
    Update {
        work_id: String,
        run_id: String,
        text: String,
    },
    /// The prompt turn settled.
    Finished {
        run_id: String,
        outcome: AcpOutcome,
        summary: String,
        detail: String,
    },
    /// The connection or the process is gone.
    Closed {
        work_id: String,
        run_id: Option<String>,
        error: Option<String>,
        confirmed: bool,
        pids: Vec<u32>,
    },
    /// A user-visible sandbox or capability notice.
    Warning { work_id: String, message: String },
}

/// Commands the ECS sends to one ACP session task.
#[derive(Debug)]
enum Command {
    Prompt { run_id: String, text: String },
    Cancel { run_id: String },
    ForceKill,
    Shutdown,
}

/// The session-level handle to one ACP agent process.
pub(crate) struct AcpHandle {
    pub(crate) work_id: String,
    pid: Arc<AtomicU32>,
    commands: UnboundedSender<Command>,
}

impl AcpHandle {
    pub(crate) fn pid(&self) -> Vec<u32> {
        let pid = self.pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid).into_iter().collect()
    }

    /// Queues the instruction of `run_id`; false when the task is gone.
    pub(crate) fn prompt(&self, run_id: &str, text: &str) -> bool {
        self.commands
            .send(Command::Prompt {
                run_id: run_id.to_owned(),
                text: text.to_owned(),
            })
            .is_ok()
    }

    /// Sends ACP `session/cancel` for `run_id`; false when the task is gone.
    pub(crate) fn cancel(&self, run_id: &str) -> bool {
        self.commands
            .send(Command::Cancel {
                run_id: run_id.to_owned(),
            })
            .is_ok()
    }

    /// Asks the backend to kill the process group; false when it is gone.
    pub(crate) fn force_kill(&self) -> bool {
        self.commands.send(Command::ForceKill).is_ok()
    }

    /// Ends the session and terminates the process group.
    pub(crate) fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// The live ACP sessions, keyed by work session id.
#[derive(Resource, Default)]
pub(crate) struct AcpBackends {
    sessions: HashMap<String, AcpHandle>,
}

impl AcpBackends {
    pub(crate) fn handle(&self, work_id: &str) -> Option<&AcpHandle> {
        self.sessions.get(work_id)
    }

    pub(crate) fn is_acp(&self, work_id: &str) -> bool {
        self.sessions.contains_key(work_id)
    }

    pub(crate) fn insert(&mut self, handle: AcpHandle) {
        self.sessions.insert(handle.work_id.clone(), handle);
    }

    pub(crate) fn remove(&mut self, work_id: &str) -> Option<AcpHandle> {
        self.sessions.remove(work_id)
    }
}

/// The backend -> ECS event channel.
#[derive(Resource)]
pub(crate) struct AcpEvents {
    sender: Sender<AcpEvent>,
    receiver: Receiver<AcpEvent>,
}

impl Default for AcpEvents {
    fn default() -> Self {
        let (sender, receiver) = unbounded();
        Self { sender, receiver }
    }
}

impl AcpEvents {
    pub(crate) fn sender(&self) -> Sender<AcpEvent> {
        self.sender.clone()
    }

    fn drain(&self) -> Vec<AcpEvent> {
        self.receiver.try_iter().collect()
    }
}

/// Starts one ACP agent for `work_id`.
///
/// A missing command is an immediate, honest failure: no run is left looking
/// alive, and no success is fabricated for a process that never started.
pub(crate) fn spawn(
    world: &mut World,
    work_id: &str,
    connection: &AcpConnection,
) -> Result<AcpHandle, HostError> {
    let command = connection.command.trim();
    if command.is_empty() {
        return Err(HostError::invalid("the ACP connection has no command"));
    }
    which::which(command).map_err(|_| {
        HostError::unavailable(format!("ACP command `{command}` was not found in PATH"))
    })?;
    let cwd = std::env::current_dir()
        .map_err(|error| HostError::unavailable(format!("ACP working directory: {error}")))?;
    let plan = sandbox::plan(
        command,
        &connection.args,
        &cwd,
        &connection.endpoint.options,
    );
    let events = world.resource::<AcpEvents>().sender();
    if !plan.was_isolated() {
        let _ = events.send(AcpEvent::Warning {
            work_id: work_id.to_owned(),
            message: plan.notice.clone(),
        });
    }
    let pid = Arc::new(AtomicU32::new(0));
    let (commands, receiver) = unbounded_channel();
    let handle = AcpHandle {
        work_id: work_id.to_owned(),
        pid: Arc::clone(&pid),
        commands,
    };
    let runtime = runtime()?;
    runtime.spawn(run_backend(
        work_id.to_owned(),
        plan,
        cwd,
        events,
        pid,
        receiver,
    ));
    Ok(handle)
}

/// Sends the ACP `session/cancel` of `run_id` if this session is ACP.
pub(crate) fn cancel(world: &World, work_id: &str, run_id: &str) -> bool {
    world
        .resource::<AcpBackends>()
        .handle(work_id)
        .is_some_and(|handle| handle.cancel(run_id))
}

/// Asks the backend to kill its process group; false when it has no task.
pub(crate) fn force_kill(world: &World, work_id: &str) -> bool {
    world
        .resource::<AcpBackends>()
        .handle(work_id)
        .is_some_and(AcpHandle::force_kill)
}

/// The PIDs to name when a stop could not be verified.
pub(crate) fn pids(world: &World, work_id: &str) -> Vec<u32> {
    world
        .resource::<AcpBackends>()
        .handle(work_id)
        .map(AcpHandle::pid)
        .unwrap_or_default()
}

/// Ends the ACP session: the process group is terminated by the backend.
pub(crate) fn shutdown(world: &mut World, work_id: &str) {
    if let Some(handle) = world.resource_mut::<AcpBackends>().remove(work_id) {
        handle.shutdown();
    }
}

/// Drains backend events into the run lifecycle.
pub(crate) fn pump(world: &mut World) {
    let events = world.resource::<AcpEvents>().drain();
    for event in events {
        match event {
            AcpEvent::Ready { run_id } => {
                super::mark_run_running(world, &run_id);
            }
            AcpEvent::Update {
                work_id,
                run_id,
                text,
            } => super::acp_update(world, &work_id, &run_id, &text),
            AcpEvent::Finished {
                run_id,
                outcome,
                summary,
                detail,
            } => {
                let outcome = match outcome {
                    AcpOutcome::Completed => {
                        let summary = if summary.trim().is_empty() {
                            "作業が完了しました".to_owned()
                        } else {
                            summary
                        };
                        super::TurnOutcome::Answered(Message::assistant(summary))
                    }
                    AcpOutcome::Cancelled => super::TurnOutcome::Stopped(Some(
                        "the ACP agent cancelled the prompt".to_owned(),
                    )),
                    AcpOutcome::Failed => super::TurnOutcome::Failed(detail),
                };
                super::settle_run(world, run_id, outcome);
            }
            AcpEvent::Closed {
                work_id,
                run_id,
                error,
                confirmed,
                pids,
            } => super::acp_closed(world, &work_id, run_id, error, confirmed, pids),
            AcpEvent::Warning { work_id, message } => super::acp_warning(world, &work_id, &message),
        }
    }
}

fn runtime() -> Result<&'static tokio::runtime::Runtime, HostError> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    match RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("ene-acp")
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(runtime) => Ok(runtime),
        Err(error) => Err(HostError::unavailable(format!(
            "the ACP runtime could not start: {error}"
        ))),
    }
}

async fn run_backend(
    work_id: String,
    plan: sandbox::SandboxPlan,
    cwd: PathBuf,
    events: Sender<AcpEvent>,
    pid: Arc<AtomicU32>,
    commands: UnboundedReceiver<Command>,
) {
    let mut wrap = CommandWrap::with_new(&plan.program, |cmd| {
        cmd.args(&plan.args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);
    wrap.wrap(KillOnDrop);
    let mut child = match wrap.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = events.send(AcpEvent::Closed {
                work_id,
                run_id: None,
                error: Some(format!("the ACP agent could not start: {error}")),
                confirmed: true,
                pids: Vec::new(),
            });
            return;
        }
    };
    if let Some(id) = child.id() {
        pid.store(id, Ordering::SeqCst);
    }
    let (Some(stdin), Some(stdout)) = (child.stdin().take(), child.stdout().take()) else {
        let _ = events.send(AcpEvent::Closed {
            work_id,
            run_id: None,
            error: Some("the ACP agent has no standard input/output".to_owned()),
            confirmed: true,
            pids: Vec::new(),
        });
        return;
    };
    if let Some(stderr) = child.stderr().take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "ene_host::acp", %line, "ACP agent stderr");
            }
        });
    }
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let shared = Arc::new(SharedRun::default());
    let notify_events = events.clone();
    let notify_shared = Arc::clone(&shared);
    let notify_work = work_id.clone();
    let result = Client
        .v1()
        .name("ene-host")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let Some((text, is_message)) = update_text(&notification.update)
                    && let Some(run_id) = notify_shared.run_id()
                {
                    if is_message {
                        notify_shared.append(&text);
                    }
                    let _ = notify_events.send(AcpEvent::Update {
                        work_id: notify_work.clone(),
                        run_id,
                        text,
                    });
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async |_request: RequestPermissionRequest, responder, _cx| {
                // Alpha has no ACP permission UI; refuse rather than pretend
                // the user approved an operation she never saw.
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            on_receive_request!(),
        )
        .connect_with(transport, {
            let events = events.clone();
            let shared = Arc::clone(&shared);
            async move |cx: ConnectionTo<Agent>| {
                session_loop(cx, cwd, events, shared, commands).await
            }
        })
        .await;
    let termination = terminate(&mut child, KILL_TIMEOUT).await;
    let _ = events.send(AcpEvent::Closed {
        work_id,
        run_id: shared.run_id(),
        error: result.err().map(|error| error.to_string()),
        confirmed: termination.confirmed,
        pids: termination.pids,
    });
}

async fn session_loop(
    cx: ConnectionTo<Agent>,
    cwd: PathBuf,
    events: Sender<AcpEvent>,
    shared: Arc<SharedRun>,
    mut commands: UnboundedReceiver<Command>,
) -> Result<(), agent_client_protocol::Error> {
    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;
    let session = cx
        .send_request(NewSessionRequest::new(cwd))
        .block_task()
        .await?;
    let session_id = session.session_id;
    let mut ready = false;
    let terminated = cx.terminated();
    tokio::pin!(terminated);
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    return Ok(());
                };
                match command {
                    Command::Prompt { run_id, text } => {
                        if !ready {
                            ready = true;
                            let _ = events.send(AcpEvent::Ready {
                                run_id: run_id.clone(),
                            });
                        }
                        serve(
                            &cx,
                            &session_id,
                            run_id,
                            text,
                            &events,
                            &shared,
                            &mut commands,
                        )
                        .await?;
                    }
                    Command::Cancel { .. } => {}
                    Command::ForceKill | Command::Shutdown => return Ok(()),
                }
            }
            _ = &mut terminated => return Ok(()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    cx: &ConnectionTo<Agent>,
    session_id: &agent_client_protocol::v1::schema::SessionId,
    run_id: String,
    text: String,
    events: &Sender<AcpEvent>,
    shared: &SharedRun,
    commands: &mut UnboundedReceiver<Command>,
) -> Result<(), agent_client_protocol::Error> {
    shared.begin(&run_id);
    let request = PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(text))],
    );
    let prompt = cx.send_request(request).block_task();
    tokio::pin!(prompt);
    let terminated = cx.terminated();
    tokio::pin!(terminated);
    loop {
        tokio::select! {
            _ = &mut terminated => {
                // Leave the run bound: `Closed` reports it and settles Failed.
                return Ok(());
            }
            result = &mut prompt => {
                let summary = shared.finish();
                let (outcome, detail) = match result {
                    Ok(response) => match response.stop_reason {
                        StopReason::EndTurn => (AcpOutcome::Completed, String::new()),
                        StopReason::Cancelled => (AcpOutcome::Cancelled, String::new()),
                        other => (
                            AcpOutcome::Failed,
                            format!("the ACP agent stopped the prompt: {other:?}"),
                        ),
                    },
                    Err(error) => (
                        AcpOutcome::Failed,
                        format!("the ACP prompt failed: {error}"),
                    ),
                };
                let _ = events.send(AcpEvent::Finished {
                    run_id: run_id.clone(),
                    outcome,
                    summary,
                    detail,
                });
                return Ok(());
            }
            command = commands.recv() => {
                match command {
                    Some(Command::Cancel { run_id: target }) => {
                        if target == run_id {
                            cx.send_notification(CancelNotification::new(session_id.clone()))?;
                        }
                    }
                    Some(Command::ForceKill | Command::Shutdown) | None => return Ok(()),
                    Some(Command::Prompt { .. }) => {
                        // One open run per session; the ECS never queues a
                        // second prompt before the first settles.
                    }
                }
            }
        }
    }
}

/// The run a streamed update belongs to, and the text that becomes the
/// completion summary.
#[derive(Debug, Default)]
struct SharedRun {
    state: Mutex<RunSlot>,
}

#[derive(Debug, Default)]
struct RunSlot {
    run_id: Option<String>,
    summary: String,
}

impl SharedRun {
    fn begin(&self, run_id: &str) {
        let mut slot = lock(&self.state);
        slot.run_id = Some(run_id.to_owned());
        slot.summary.clear();
    }

    fn run_id(&self) -> Option<String> {
        lock(&self.state).run_id.clone()
    }

    fn append(&self, text: &str) {
        let mut slot = lock(&self.state);
        if slot.summary.chars().count() >= 8_000 {
            return;
        }
        slot.summary.push_str(text);
    }

    fn finish(&self) -> String {
        let mut slot = lock(&self.state);
        slot.run_id = None;
        std::mem::take(&mut slot.summary).trim().to_owned()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn update_text(update: &SessionUpdate) -> Option<(String, bool)> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            content_text(&chunk.content).map(|text| (text, true))
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            content_text(&chunk.content).map(|text| (text, false))
        }
        _ => None,
    }
}

fn content_text(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => (!text.text.trim().is_empty()).then(|| text.text.clone()),
        _ => None,
    }
}

/// What happened to the child after the connection ended.
#[derive(Debug)]
pub(crate) struct Termination {
    pub(crate) confirmed: bool,
    pub(crate) pids: Vec<u32>,
}

/// Kills the process group and reports whether every process in it is gone.
pub(crate) async fn terminate(child: &mut Box<dyn ChildWrapper>, timeout: Duration) -> Termination {
    let pids: Vec<u32> = child.id().into_iter().collect();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Termination {
            confirmed: true,
            pids,
        };
    }
    // `start_kill` on the process-group wrapper signals the whole group;
    // `wait` reaps it. Both are needed to call the stop confirmed.
    if child.start_kill().is_err() {
        return Termination {
            confirmed: matches!(child.try_wait(), Ok(Some(_))),
            pids,
        };
    }
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    match waited {
        Ok(Ok(_)) => Termination {
            confirmed: true,
            pids,
        },
        _ => Termination {
            confirmed: false,
            pids,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_updates_become_progress() {
        let text = ContentBlock::Text(TextContent::new("cargo test"));
        assert_eq!(content_text(&text).as_deref(), Some("cargo test"));
        assert!(super::super::sandbox::requested_mode("{}").is_none());
    }

    #[test]
    fn the_summary_is_scoped_to_one_run() {
        let shared = SharedRun::default();
        shared.begin("run-a");
        shared.append("done ");
        shared.append("well");
        assert_eq!(shared.run_id().as_deref(), Some("run-a"));
        assert_eq!(shared.finish(), "done well");
        assert!(shared.run_id().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_forced_kill_confirms_the_process_group_is_gone() {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        runtime.block_on(async {
            let mut wrap = CommandWrap::with_new("sh", |cmd| {
                cmd.arg("-c")
                    .arg("sleep 30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            });
            wrap.wrap(ProcessGroup::leader());
            wrap.wrap(KillOnDrop);
            let mut child = wrap.spawn().expect("the sleeping child starts");
            let termination = terminate(&mut child, Duration::from_secs(5)).await;
            assert!(
                termination.confirmed,
                "the process group is killed and reaped: {termination:?}"
            );
            assert!(!termination.pids.is_empty());
        });
    }
}
