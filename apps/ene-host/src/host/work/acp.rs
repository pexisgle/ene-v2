//! The external ACP backend (tech-stack.md §G): one process per WorkSession,
//! one `session/prompt` turn per Run. Commands go to [`AcpHandle`]; [`AcpEvents`]
//! carries results back to [`pump`], which settles them like the rig backend.
use super::acp_bridge::{self, Bridge};
use crate::host::{
    db::{Store, truncate},
    error::HostError,
    notify::Notifier,
    resources::HostRuntime,
    settings::AcpConnection,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, on_receive_notification, on_receive_request,
    schema::ProtocolVersion,
    v1::{
        ConnectionTo,
        schema::{
            CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
        },
    },
};
use bevy_ecs::prelude::*;

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rig_core::completion::Message;
use rig_ecs::prelude::TurnOutcome;
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// How many ACP events may queue before a backend waits. A child that
/// floods notifications is slowed instead of grown.
const QUEUE: usize = 256;

/// How many commands may wait for the backend before a control call reports
/// that it could not be delivered.
const COMMANDS: usize = 64;

/// How long the child gets to exit after `SIGTERM` before it is killed. Only
/// the Unix stop asks first; other targets kill directly.
#[cfg(unix)]
pub(super) const TERM_GRACE: Duration = Duration::from_secs(2);

/// Longest a forced kill may take to be reaped; `work::FORCE_GRACE` is the
/// wait that must outlast the whole stop before a stop counts as
/// unconfirmed. On Unix that is `TERM_GRACE` plus this; elsewhere it is this
/// alone.
pub(super) const KILL_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum AcpEvent {
    Ready {
        run_id: String,
    },
    Update {
        work_id: String,
        run_id: String,
        text: String,
    },
    Finished {
        run_id: String,
        outcome: TurnOutcome,
    },
    Closed {
        work_id: String,
        run_id: Option<String>,
        error: Option<String>,
        confirmed: bool,
        pids: Vec<u32>,
    },
}

enum Command {
    Prompt { run_id: String, text: String },
    Cancel { run_id: String },
    End,
}

pub(crate) struct AcpHandle {
    pid: Arc<AtomicU32>,
    commands: mpsc::Sender<Command>,
    stop: tokio_util::sync::CancellationToken,
}

impl AcpHandle {
    pub(crate) fn pid(&self) -> Vec<u32> {
        let pid = self.pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid).into_iter().collect()
    }

    pub(crate) fn prompt(&self, run_id: &str, text: &str) -> bool {
        self.commands
            .try_send(Command::Prompt {
                run_id: run_id.to_owned(),
                text: text.to_owned(),
            })
            .is_ok()
    }

    pub(crate) fn cancel(&self, run_id: &str) -> bool {
        self.commands
            .try_send(Command::Cancel {
                run_id: run_id.to_owned(),
            })
            .is_ok()
    }

    pub(crate) fn end(&self) -> bool {
        let accepting = !self.commands.is_closed();
        self.stop.cancel();
        let _ = self.commands.try_send(Command::End);
        accepting
    }
}

impl Drop for AcpHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Resource, Default)]
pub(crate) struct AcpBackends(pub(crate) HashMap<String, AcpHandle>);

#[derive(Resource)]
pub(crate) struct AcpEvents {
    sender: mpsc::Sender<AcpEvent>,
    receiver: mpsc::Receiver<AcpEvent>,
}

impl Default for AcpEvents {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self { sender, receiver }
    }
}

impl AcpEvents {
    fn sender(&self) -> mpsc::Sender<AcpEvent> {
        self.sender.clone()
    }

    fn drain(&mut self) -> Vec<AcpEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            out.push(event);
        }
        out
    }
}

pub(crate) fn spawn(
    world: &mut World,
    work_id: &str,
    run_id: &str,
    connection: &AcpConnection,
) -> Result<AcpHandle, HostError> {
    let connection = connection.clone();
    let command = connection.command.trim();
    if command.is_empty() {
        return Err(HostError::Invalid(
            ("the ACP connection has no command").into(),
        ));
    }
    let cwd = std::env::current_dir()
        .map_err(|error| HostError::Unavailable(format!("ACP working directory: {error}")))?;
    // Resolve against the PATH the child will actually receive, not the
    // host's: a configured `env.PATH` replaces the inherited one.
    let program = resolve_program(command, &connection.endpoint.options, &cwd)?;
    let bwrap = which::which("bwrap").ok();
    let preparation = bridge(&connection, bwrap.as_deref())?;
    let bridge = preparation.as_ref().map(|(socket, _)| Bridge {
        socket: socket.clone(),
        port: acp_bridge::RELAY_PORT,
    });
    let Launch {
        program,
        args,
        isolation,
    } = launch(
        &program,
        &connection.args,
        &cwd,
        &connection.endpoint.options,
        bwrap.as_deref(),
        bridge.as_ref(),
    )
    .map_err(HostError::Unavailable)?;
    if let Some((code, notice)) = isolation.notice() {
        let store = world.resource::<Store>().clone();
        if let Ok(Some(session)) = store.work_session(work_id) {
            world.resource_mut::<Notifier>().notice(
                Some(&session.companion_id),
                code,
                &truncate(notice, 500),
            );
        }
    }
    let events = world.resource::<AcpEvents>().sender();
    let pid = Arc::new(AtomicU32::new(0));
    let (commands, receiver) = mpsc::channel(COMMANDS);
    let stop = tokio_util::sync::CancellationToken::new();
    let handle = AcpHandle {
        pid: Arc::clone(&pid),
        commands,
        stop: stop.clone(),
    };
    let mut env = child_env(&connection.endpoint.options, |name: &str| {
        std::env::var_os(name)
    });
    // A sandboxed child reaches its model only through the proxy.
    if let Some(bridge) = &bridge {
        for name in acp_bridge::PROXY_VARS {
            env.retain(|(key, _)| key != name);
            env.push((OsString::from(name), OsString::from(bridge.url())));
        }
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|rt| rt.0.clone()) else {
        return Err(HostError::Unavailable(
            ("the host runtime is unavailable").into(),
        ));
    };
    runtime.spawn(run_backend(
        BackendIdentity {
            work_id: work_id.to_owned(),
            initial_run_id: run_id.to_owned(),
            pid,
        },
        command_wrap(&program, &args, &cwd, env),
        cwd,
        events,
        receiver,
        preparation,
        stop,
    ));
    Ok(handle)
}

pub(crate) fn cancel(world: &World, work_id: &str, run_id: &str) -> bool {
    let backends = &world.resource::<AcpBackends>().0;
    backends
        .get(work_id)
        .is_some_and(|handle| handle.cancel(run_id))
}

pub(crate) fn force_kill(world: &World, work_id: &str) -> bool {
    let backends = &world.resource::<AcpBackends>().0;
    backends.get(work_id).is_some_and(AcpHandle::end)
}

pub(crate) fn pids(world: &World, work_id: &str) -> Vec<u32> {
    let backends = &world.resource::<AcpBackends>().0;
    backends
        .get(work_id)
        .map(AcpHandle::pid)
        .unwrap_or_default()
}

pub(crate) fn shutdown(world: &mut World, work_id: &str) {
    if let Some(handle) = world.resource_mut::<AcpBackends>().0.remove(work_id) {
        handle.end();
    }
}

pub(crate) fn pump(world: &mut World) {
    for event in world.resource_mut::<AcpEvents>().drain() {
        match event {
            AcpEvent::Ready { run_id } => super::mark_run_running(world, &run_id),
            AcpEvent::Update {
                work_id,
                run_id,
                text,
            } => super::acp_update(world, &work_id, &run_id, &text),
            AcpEvent::Finished { run_id, outcome } => super::settle_run(world, run_id, outcome),
            AcpEvent::Closed {
                work_id,
                run_id,
                error,
                confirmed,
                pids,
            } => super::acp_closed(world, &work_id, run_id, error, confirmed, pids),
        }
    }
}

fn closed(work_id: String, run_id: Option<String>, error: String) -> AcpEvent {
    AcpEvent::Closed {
        work_id,
        run_id,
        error: Some(error),
        confirmed: true,
        pids: Vec::new(),
    }
}

fn command_wrap(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: Vec<(OsString, OsString)>,
) -> CommandWrap {
    let mut wrap = CommandWrap::with_new(program, |cmd| {
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);
    wrap.wrap(KillOnDrop);
    wrap
}

struct BackendIdentity {
    work_id: String,
    initial_run_id: String,
    pid: Arc<AtomicU32>,
}

async fn run_backend(
    identity: BackendIdentity,
    mut wrap: CommandWrap,
    cwd: PathBuf,
    events: mpsc::Sender<AcpEvent>,
    commands: mpsc::Receiver<Command>,
    preparation: Option<(PathBuf, acp_bridge::ConfiguredOrigins)>,
    stop: tokio_util::sync::CancellationToken,
) {
    let BackendIdentity {
        work_id,
        initial_run_id,
        pid,
    } = identity;
    let shared = Arc::new(SharedRun::default());
    shared.begin(&initial_run_id);
    let proxy = if let Some((socket, configured)) = preparation {
        let prepared = tokio::select! {
            biased;
            _ = stop.cancelled() => {
                let _ = events.send(AcpEvent::Closed {
                    work_id, run_id: shared.run_id(), error: None, confirmed: true, pids: Vec::new(),
                }).await;
                return;
            }
            prepared = configured.pin() => prepared,
        };
        let proxy = prepared.and_then(|policy| {
            acp_bridge::open(&tokio::runtime::Handle::current(), socket, policy)
        });
        match proxy {
            Ok(proxy) => Some(proxy),
            Err(error) => {
                let _ = events
                    .send(closed(
                        work_id,
                        shared.run_id(),
                        format!("the isolated ACP model proxy could not start: {error}"),
                    ))
                    .await;
                return;
            }
        }
    } else {
        None
    };
    if stop.is_cancelled() {
        drop(proxy);
        let _ = events
            .send(AcpEvent::Closed {
                work_id,
                run_id: shared.run_id(),
                error: None,
                confirmed: true,
                pids: Vec::new(),
            })
            .await;
        return;
    }
    let mut child = match wrap.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(proxy);
            let _ = events
                .send(closed(
                    work_id,
                    shared.run_id(),
                    format!("the ACP agent could not start: {error}"),
                ))
                .await;
            return;
        }
    };
    if let Some(id) = child.id() {
        pid.store(id, Ordering::SeqCst);
    }
    let (Some(stdin), Some(stdout)) = (child.stdin().take(), child.stdout().take()) else {
        drop(proxy);
        let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
        let _ = events
            .send(AcpEvent::Closed {
                work_id,
                run_id: shared.run_id(),
                error: Some("the ACP agent has no standard input/output".to_owned()),
                confirmed,
                pids,
            })
            .await;
        return;
    };
    let mut owned = tokio::task::JoinSet::new();
    if let Some(stderr) = child.stderr().take() {
        owned.spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "ene_host::acp", %line, "ACP agent stderr");
            }
        });
    }
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let error = {
        let connection = connect(
            transport,
            cwd,
            events.clone(),
            work_id.clone(),
            Arc::clone(&shared),
            commands,
        );
        tokio::pin!(connection);
        tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            error = &mut connection => error,
            result = observe_exit(&mut child) => Some(match result {
                Ok(status) => format!("the ACP agent exited: {status}"),
                Err(error) => format!("waiting for the ACP agent failed: {error}"),
            }),
        }
    };
    drop(proxy);
    owned.abort_all();
    while owned.join_next().await.is_some() {}
    let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
    let _ = events
        .send(AcpEvent::Closed {
            work_id,
            run_id: shared.run_id(),
            error,
            confirmed,
            pids,
        })
        .await;
}

/// Runs the client side of one ACP connection until the transport ends or the
/// session is told to stop. Returns the reason the connection failed, if it did.
async fn connect<T>(
    transport: T,
    cwd: PathBuf,
    events: mpsc::Sender<AcpEvent>,
    work_id: String,
    shared: Arc<SharedRun>,
    commands: mpsc::Receiver<Command>,
) -> Option<String>
where
    T: agent_client_protocol::ConnectTo<Client> + 'static,
{
    let notify_events = events.clone();
    let notify_shared = Arc::clone(&shared);
    let notify_work = work_id;
    Client
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
                    let _ = notify_events
                        .send(AcpEvent::Update {
                            work_id: notify_work.clone(),
                            run_id,
                            text,
                        })
                        .await;
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
        .await
        .err()
        .map(|error| error.to_string())
}

async fn session_loop(
    cx: ConnectionTo<Agent>,
    cwd: PathBuf,
    events: mpsc::Sender<AcpEvent>,
    shared: Arc<SharedRun>,
    mut commands: mpsc::Receiver<Command>,
) -> Result<(), agent_client_protocol::Error> {
    let Some((session_id, first)) = open(&cx, &cwd, &shared, &mut commands).await? else {
        return Ok(());
    };
    let mut ready = false;
    if let Some((run_id, text)) = first {
        announce(&mut ready, &events, &run_id).await;
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
                        announce(&mut ready, &events, &run_id).await;
                        serve(&cx, &session_id, run_id, text, &events, &shared, &mut commands).await?;
                    }
                    Command::Cancel { .. } => {}
                    Command::End => return Ok(()),
                }
            }
            _ = &mut terminated => return Ok(()),
        }
    }
}

async fn open(
    cx: &ConnectionTo<Agent>,
    cwd: &Path,
    shared: &SharedRun,
    commands: &mut mpsc::Receiver<Command>,
) -> Result<Option<(SessionId, Option<(String, String)>)>, agent_client_protocol::Error> {
    let initialize = cx
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task();
    tokio::pin!(initialize);
    let mut first = None;
    loop {
        tokio::select! {
            result = &mut initialize => {
                result?;
                break;
            }
            command = commands.recv() => match command {
                Some(Command::Prompt { run_id, text }) => {
                    first.get_or_insert((run_id, text));
                }
                Some(Command::Cancel { run_id }) => {
                    if shared.run_id().as_deref() == Some(&run_id) {
                        return Ok(None);
                    }
                }
                Some(Command::End) | None => return Ok(None),
            }
        }
    }
    let session = cx
        .send_request(NewSessionRequest::new(cwd.to_path_buf()))
        .block_task();
    tokio::pin!(session);
    loop {
        tokio::select! {
            result = &mut session => {
                let session = result?;
                return Ok(Some((session.session_id, first)));
            }
            command = commands.recv() => match command {
                Some(Command::Prompt { run_id, text }) => {
                    first.get_or_insert((run_id, text));
                }
                Some(Command::Cancel { run_id }) => {
                    if shared.run_id().as_deref() == Some(&run_id) {
                        return Ok(None);
                    }
                }
                Some(Command::End) | None => return Ok(None),
            }
        }
    }
}

async fn announce(ready: &mut bool, events: &mpsc::Sender<AcpEvent>, run_id: &str) {
    if !*ready {
        *ready = true;
        let _ = events
            .send(AcpEvent::Ready {
                run_id: run_id.to_owned(),
            })
            .await;
    }
}

async fn serve(
    cx: &ConnectionTo<Agent>,
    session_id: &SessionId,
    run_id: String,
    text: String,
    events: &mpsc::Sender<AcpEvent>,
    shared: &SharedRun,
    commands: &mut mpsc::Receiver<Command>,
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
                let outcome = match result {
                    Ok(response) => match response.stop_reason {
                        StopReason::EndTurn => TurnOutcome::Answered(Message::assistant(
                            if summary.trim().is_empty() {
                                "作業が完了しました".to_owned()
                            } else {
                                summary
                            },
                        )),
                        StopReason::Cancelled => TurnOutcome::Stopped(Some(
                            "the ACP agent cancelled the prompt".to_owned(),
                        )),
                        other => TurnOutcome::Failed(format!(
                            "the ACP agent stopped the prompt: {other:?}"
                        )),
                    },
                    Err(error) => TurnOutcome::Failed(format!("the ACP prompt failed: {error}")),
                };
                let _ = events
                    .send(AcpEvent::Finished { run_id, outcome })
                    .await;
                return Ok(());
            }
            command = commands.recv() => {
                match command {
                    Some(Command::Cancel { run_id: target }) if target == run_id => {
                        cx.send_notification(CancelNotification::new(session_id.clone()))?;
                    }
                    Some(Command::Cancel { .. } | Command::Prompt { .. }) => {}
                    Some(Command::End) | None => return Ok(()),
                }
            }
        }
    }
}

#[derive(Default)]
struct SharedRun {
    state: Mutex<RunSlot>,
}

#[derive(Default)]
struct RunSlot {
    run_id: Option<String>,
    summary: String,
}

impl SharedRun {
    fn begin(&self, run_id: &str) {
        let mut slot = crate::host::lock(&self.state);
        slot.run_id = Some(run_id.to_owned());
        slot.summary.clear();
    }

    fn run_id(&self) -> Option<String> {
        crate::host::lock(&self.state).run_id.clone()
    }

    fn append(&self, text: &str) {
        let mut slot = crate::host::lock(&self.state);
        if slot.summary.chars().count() < 8_000 {
            slot.summary.push_str(text);
        }
    }

    fn finish(&self) -> String {
        let mut slot = crate::host::lock(&self.state);
        slot.run_id = None;
        std::mem::take(&mut slot.summary).trim().to_owned()
    }
}

fn update_text(update: &SessionUpdate) -> Option<(String, bool)> {
    let (content, is_message) = match update {
        SessionUpdate::AgentMessageChunk(chunk) => (&chunk.content, true),
        SessionUpdate::AgentThoughtChunk(chunk) => (&chunk.content, false),
        _ => return None,
    };
    let ContentBlock::Text(text) = content else {
        return None;
    };
    (!text.text.trim().is_empty()).then(|| (text.text.clone(), is_message))
}

#[cfg(unix)]
async fn observe_exit(child: &mut Box<dyn ChildWrapper>) -> std::io::Result<String> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::other("the ACP leader is no longer owned"))?;
    loop {
        // Keeping the leader waitable reserves its PID/PGID until group kill.
        if let Some(status) = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )? {
            return Ok(format!("{status:?}"));
        }
        if exits.recv().await.is_none() {
            return Err(std::io::Error::other("the child-exit signal stream ended"));
        }
    }
}

#[cfg(not(unix))]
async fn observe_exit(child: &mut Box<dyn ChildWrapper>) -> std::io::Result<String> {
    child.wait().await.map(|status| status.to_string())
}

/// Stops the child, asking it to exit before forcing it.
///
/// The child leads its own process group (see [`command_wrap`]), and
/// `ChildWrapper::signal` delivers to that whole group, so a wrapper launcher
/// cannot leave the real agent behind. On Unix `SIGTERM` first gives the agent
/// its documented chance to flush and exit, and whatever is still running
/// after `TERM_GRACE` is killed; other targets have no such signal and kill
/// directly. `timeout` is the budget for the kill that follows, so the
/// caller's own grace must outlast the whole stop.
async fn terminate(child: &mut Box<dyn ChildWrapper>, timeout: Duration) -> (bool, Vec<u32>) {
    let pids: Vec<u32> = child.id().into_iter().collect();
    #[cfg(unix)]
    if pids.is_empty() {
        return (matches!(child.try_wait(), Ok(Some(_))), pids);
    }
    #[cfg(unix)]
    if child.signal(rustix::process::Signal::TERM.as_raw()).is_ok() {
        // A process that exits on the signal is already reaped by this wait.
        if matches!(
            tokio::time::timeout(TERM_GRACE, child.wait()).await,
            Ok(Ok(_))
        ) {
            return (true, pids);
        }
    }
    let signalled = match child.start_kill() {
        Ok(()) => true,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => true,
        Err(_) => false,
    };
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    (signalled && matches!(waited, Ok(Ok(_))), pids)
}

// === Child environment ====================================================

/// The only parent variables an ACP child inherits; provider keys and every
/// other secret stay out.
#[cfg(windows)]
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "SYSTEMROOT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "COMSPEC",
    "PATHEXT",
];
#[cfg(not(windows))]
const INHERITED_ENV: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"];

fn child_env(
    options: &str,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let mut envs: Vec<(OsString, OsString)> = INHERITED_ENV
        .iter()
        .filter_map(|name| lookup(name).map(|value| (OsString::from(name), value)))
        .collect();
    for (name, value) in configured_env(options) {
        let name = OsString::from(name);
        match envs
            .iter_mut()
            .find(|entry| entry.0.as_os_str() == name.as_os_str())
        {
            Some((_, inherited)) => *inherited = OsString::from(value),
            None => envs.push((name, OsString::from(value))),
        }
    }
    envs
}

/// Values the connection options add to the child environment.
fn configured_env(options: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(options) else {
        return Vec::new();
    };
    let Some(env) = value.get("env").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    env.iter()
        .filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.to_owned())))
        .collect()
}

/// Resolves the command with the PATH the child receives: the configured
/// `env.PATH` when the connection sets one, otherwise the inherited host PATH
/// that [`child_env`] passes through.
fn resolve_program(command: &str, options: &str, cwd: &Path) -> Result<PathBuf, HostError> {
    let configured = configured_env(options)
        .into_iter()
        .find_map(|(name, value)| (name == "PATH").then_some(OsString::from(value)));
    let path = configured.or_else(|| std::env::var_os("PATH"));
    which::which_in(command, path, cwd).map_err(|_| {
        HostError::Unavailable(format!(
            "ACP command `{command}` was not found in the PATH the child receives"
        ))
    })
}

// === Process isolation ===================================================

#[derive(Debug)]
struct Launch {
    program: PathBuf,
    args: Vec<String>,
    /// What the launch actually enforces, for the notice the host publishes.
    isolation: Isolation,
}

/// How much the launched child is really isolated. A partial launch must never
/// be reported as full isolation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Isolation {
    /// `sandbox: none`: no namespace at all.
    None,
    /// The environment has no `bwrap`, so the child ran unisolated and the
    /// host says so instead of refusing to start it.
    Missing,
    /// Read-only root, private `/tmp` and a PID namespace; host network.
    Partial,
    /// The above plus a private network namespace.
    Full,
}

impl Isolation {
    /// The notice this launch owes the user, if any: an opt-out or a partial
    /// sandbox is never silently presented as full isolation.
    fn notice(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::None => Some((
                "acp_sandbox_unavailable",
                "ACP接続の `sandbox: none` オプションにより、子プロセスはファイルシステム・プロセス・ネットワークのいずれも隔離せずに起動します。",
            )),
            Self::Missing => Some((
                "acp_sandbox_unavailable",
                "この環境では bwrap が見つからないため、ACP子プロセスを隔離せずに起動します。\
                 隔離が必要な場合は bwrap を導入し、接続オプションの sandbox を確認してください。",
            )),
            Self::Partial => Some((
                "acp_sandbox_partial",
                "ACP子プロセスは読み取り専用ルート・専用の /tmp・PID名前空間で起動しますが、\
                 ネットワークはホストと共有されます。分離する場合は接続オプションに\
                 `network: isolated` を設定してください。",
            )),
            Self::Full => None,
        }
    }
}

/// The proxy a network-isolated launch needs. Only asked for when the user
/// requested isolation and bwrap is present: an unisolated child has the host
/// network and needs no bridge.
fn bridge(
    connection: &AcpConnection,
    bwrap: Option<&Path>,
) -> Result<Option<(PathBuf, acp_bridge::ConfiguredOrigins)>, HostError> {
    let options = &connection.endpoint.options;
    if sandbox_mode(options).as_deref() == Some("none")
        || !network_isolated(options).map_err(HostError::Invalid)?
        || bwrap.is_none()
    {
        return Ok(None);
    }
    let configured = acp_bridge::ConfiguredOrigins::parse(&connection.endpoint.endpoint, options)
        .map_err(|error| HostError::Invalid(error.to_string()))?;
    let socket = std::env::temp_dir().join(format!(
        "ene-acp-{}.sock",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    Ok(Some((socket, configured)))
}

fn launch(
    program: &Path,
    args: &[String],
    cwd: &Path,
    options: &str,
    bwrap: Option<&Path>,
    bridge: Option<&Bridge>,
) -> Result<Launch, String> {
    let requested = sandbox_mode(options);
    if requested.as_deref() == Some("none") {
        return Ok(Launch {
            program: program.to_path_buf(),
            args: args.to_vec(),
            isolation: Isolation::None,
        });
    }
    plan(
        program,
        args,
        cwd,
        bwrap,
        network_isolated(options)?,
        bridge,
    )
}

/// Builds the bwrap command line. The system stays read-only (`--ro-bind / /`),
/// the working directory is the only writable bind, and `/tmp` is a private
/// tmpfs so agents can write temporary files without touching the host's.
///
/// Network isolation is opt-in: `--unshare-net` is passed only for an explicit
/// `network: isolated` (or `network: none`), because there is no proxy bridge
/// that would let a network-isolated child reach its model or tools. Without
/// it the child shares the host network and the launch is
/// [`Isolation::Partial`]. Without bwrap the child cannot be isolated at all,
/// so it runs unisolated as [`Isolation::Missing`] and the host says so.
fn plan(
    program: &Path,
    args: &[String],
    cwd: &Path,
    bwrap: Option<&Path>,
    network_isolated: bool,
    bridge: Option<&Bridge>,
) -> Result<Launch, String> {
    let Some(bwrap) = bwrap else {
        // A missing sandbox is a written warning, not a refusal: the user asked
        // for an agent, and the notice says what it ran without.
        return Ok(Launch {
            program: program.to_path_buf(),
            args: args.to_vec(),
            isolation: Isolation::Missing,
        });
    };
    let cwd_text = cwd
        .to_str()
        .ok_or_else(|| "ACPの作業ディレクトリをUTF-8で表せません".to_owned())?;
    let program_text = program
        .to_str()
        .ok_or_else(|| "ACPコマンドのパスをUTF-8で表せません".to_owned())?;
    let mut wrapped: Vec<String> = ["--die-with-parent", "--unshare-pid"]
        .map(str::to_owned)
        .to_vec();
    if network_isolated && bridge.is_none() {
        return Err(
            "ネットワーク分離にはプロキシブリッジが必要ですが、起動できませんでした".to_owned(),
        );
    }
    if network_isolated {
        wrapped.push("--unshare-net".to_owned());
    }
    let socket = bridge
        .map(|bridge| {
            bridge
                .socket
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "プロキシのソケットパスをUTF-8で表せません".to_owned())
        })
        .transpose()?;
    wrapped.extend(["--ro-bind", "/", "/"].map(str::to_owned));
    // Mount the private /tmp and the kernel filesystems before the work
    // directory bind: a work directory under /tmp (or /proc, /dev) must still
    // be the program's writable cwd, not be shadowed by the later mount.
    wrapped.extend(["--tmpfs", "/tmp", "--proc", "/proc", "--dev", "/dev"].map(str::to_owned));
    if let Some(socket) = &socket {
        // The socket is the child's only route out, and the private /tmp hides
        // a socket that lives there, so this bind comes after that mount.
        wrapped.extend(["--bind", socket, socket].map(str::to_owned));
    }
    wrapped.extend(["--bind", cwd_text, cwd_text].map(str::to_owned));
    wrapped.extend(["--chdir", cwd_text].map(str::to_owned));
    wrapped.push("--".to_owned());
    if let (Some(bridge), Some(socket)) = (bridge, &socket) {
        // Inside the namespace the relay is the only listener: it forwards to
        // the host's proxy and runs the agent with this process's stdio.
        let relay = std::env::current_exe()
            .map_err(|error| format!("ACPリレーの実体を解決できません: {error}"))?;
        let listen = format!("127.0.0.1:{}", bridge.port);
        wrapped.push(relay.to_string_lossy().into_owned());
        wrapped.extend(
            ["acp-relay", "--socket", socket, "--listen", &listen, "--"].map(str::to_owned),
        );
    }
    wrapped.push(program_text.to_owned());
    wrapped.extend(args.iter().cloned());
    Ok(Launch {
        program: bwrap.to_path_buf(),
        args: wrapped,
        isolation: if network_isolated {
            Isolation::Full
        } else {
            Isolation::Partial
        },
    })
}

fn sandbox_mode(options: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(options).ok()?;
    let mode = value.get("sandbox")?.as_str()?.trim().to_ascii_lowercase();
    (!mode.is_empty()).then_some(mode)
}

fn network_isolated(options: &str) -> Result<bool, String> {
    // An empty options document means the defaults: the host network.
    if options.trim().is_empty() {
        return Ok(false);
    }
    let value: serde_json::Value = serde_json::from_str(options)
        .map_err(|_| "connection options are not a JSON object".to_owned())?;
    match value.get("network") {
        None => Ok(false),
        Some(serde_json::Value::String(mode)) => match mode.trim().to_ascii_lowercase().as_str() {
            "isolated" => Ok(true),
            "host" => Ok(false),
            other => Err(format!(
                "unknown network mode `{other}`; use host or isolated"
            )),
        },
        Some(_) => Err("network must be a string".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child that handles `SIGTERM` must be asked before it is killed: a
    /// kill-only stop would leave `graceful` empty even though the process
    /// could have exited cleanly.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn terminating_an_agent_asks_before_it_kills() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let graceful = root.path().join("graceful");
        let mut command = command_wrap(
            &which::which("sh").unwrap(),
            &[
                "-c".into(),
                // The trap has to be installed before the stop arrives, or the
                // default disposition ends the shell first.
                "trap 'echo yes > \"$2\"; exit 0' TERM; echo ready > \"$1\"; \
                 while :; do sleep 0.05; done"
                    .into(),
                "ene-acp-test".into(),
                ready.to_str().unwrap().into(),
                graceful.to_str().unwrap().into(),
            ],
            root.path(),
            child_env("", |name| std::env::var_os(name)),
        );
        let mut child = command.spawn().unwrap();
        tokio::time::timeout(KILL_TIMEOUT, async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the agent installed its trap");
        let (confirmed, _pids) = terminate(&mut child, KILL_TIMEOUT).await;
        assert!(confirmed);
        assert_eq!(
            std::fs::read_to_string(&graceful).ok().as_deref(),
            Some("yes\n"),
            "the agent was killed without being asked to exit"
        );
    }

    /// A child that ignores `SIGTERM` must still be killed once the ask
    /// expires, or the stop would wait on a process that never leaves.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn terminating_an_agent_that_ignores_the_ask_still_kills_it() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let mut command = command_wrap(
            &which::which("sh").unwrap(),
            &[
                "-c".into(),
                "trap '' TERM; echo ready > \"$1\"; while :; do sleep 0.05; done".into(),
                "ene-acp-test".into(),
                ready.to_str().unwrap().into(),
            ],
            root.path(),
            child_env("", |name| std::env::var_os(name)),
        );
        let mut child = command.spawn().unwrap();
        tokio::time::timeout(KILL_TIMEOUT, async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the agent installed its trap");
        let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
        assert!(confirmed, "the escalation did not reap the agent");
        assert_eq!(pids.len(), 1);
        assert!(!Path::new(&format!("/proc/{}", pids[0])).exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn terminating_an_agent_stops_its_process_group() {
        let root = tempfile::tempdir().unwrap();
        let descendant = root.path().join("descendant");
        let mut command = command_wrap(
            &which::which("sh").unwrap(),
            &[
                "-c".into(),
                "sleep 30 & echo $! > \"$1\"; wait".into(),
                "ene-acp-test".into(),
                descendant.to_str().unwrap().into(),
            ],
            root.path(),
            child_env("", |name| std::env::var_os(name)),
        );
        let mut child = command.spawn().unwrap();
        let leader = child.id().unwrap();
        let descendant = tokio::time::timeout(KILL_TIMEOUT, async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&descendant)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the agent spawned its descendant");
        let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
        assert!(confirmed);
        assert_eq!(pids, vec![leader]);
        assert!(!Path::new(&format!("/proc/{leader}")).exists());
        tokio::time::timeout(KILL_TIMEOUT, async {
            loop {
                let status = std::fs::read_to_string(format!("/proc/{descendant}/status"));
                if status.is_err()
                    || status.is_ok_and(|status| {
                        status.lines().any(|line| line.starts_with("State:\tZ"))
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the descendant is no longer running");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_exited_agent_cannot_leave_a_running_descendant() {
        let root = tempfile::tempdir().unwrap();
        let descendant_file = root.path().join("descendant");
        let (events, mut receiver) = mpsc::channel(QUEUE);
        let (_sender, commands) = mpsc::channel(COMMANDS);
        run_backend(
            BackendIdentity {
                work_id: "work".into(),
                initial_run_id: "accepted-run".into(),
                pid: Arc::new(AtomicU32::new(0)),
            },
            command_wrap(
                &which::which("sh").unwrap(),
                &[
                    "-c".into(),
                    "sleep 30 & echo $! > \"$1\"; exit 0".into(),
                    "acp-test".into(),
                    descendant_file.to_str().unwrap().into(),
                ],
                root.path(),
                child_env("", |name| std::env::var_os(name)),
            ),
            root.path().to_owned(),
            events,
            commands,
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        let descendant: i32 = std::fs::read_to_string(descendant_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let stopped = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let status = std::fs::read_to_string(format!("/proc/{descendant}/status"));
                if status.is_err()
                    || status.is_ok_and(|status| {
                        status.lines().any(|line| line.starts_with("State:\tZ"))
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !stopped {
            let pid = rustix::process::Pid::from_raw(descendant).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        assert!(matches!(
            receiver.recv().await,
            Some(AcpEvent::Closed {
                confirmed: true,
                ..
            })
        ));
        assert!(
            stopped,
            "backend closure left its descendant {descendant} running"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_preparation_failure_settles_the_accepted_starting_run() {
        use super::super::{RunState, Runs, WorkSession, WorkStatus, new_run};
        use crate::host::agent::WorkReports;

        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        store
            .insert_work_session(&WorkSession {
                id: "work".into(),
                companion_id: "ene".into(),
                title: "task".into(),
                status: WorkStatus::Active,
                client_id: None,
                activation_revision: 0,
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap();
        let run = new_run("accepted-run".into(), "work", "hello", RunState::Starting);
        store.insert_run(&run).unwrap();
        let mut world = World::new();
        world.insert_resource(store.clone());
        world.insert_resource(Notifier::new());
        world.insert_resource(WorkReports::default());
        world.insert_resource(Runs::default());
        world.insert_resource(AcpBackends::default());
        world.insert_resource(AcpEvents::default());
        let (commands, receiver) = mpsc::channel(COMMANDS);
        let stop = tokio_util::sync::CancellationToken::new();
        let pid = Arc::new(AtomicU32::new(0));
        let handle = AcpHandle {
            pid: Arc::clone(&pid),
            commands,
            stop: stop.clone(),
        };
        assert!(handle.prompt(&run.run_id, "hello"));
        world
            .resource_mut::<AcpBackends>()
            .0
            .insert("work".into(), handle);
        let marker = root.path().join("started");
        run_backend(
            BackendIdentity {
                work_id: "work".into(),
                initial_run_id: "accepted-run".into(),
                pid,
            },
            command_wrap(
                &which::which("sh").unwrap(),
                &[
                    "-c".into(),
                    "touch \"$1\"".into(),
                    "acp-test".into(),
                    marker.to_str().unwrap().into(),
                ],
                root.path(),
                child_env("", |name| std::env::var_os(name)),
            ),
            root.path().to_owned(),
            world.resource::<AcpEvents>().sender(),
            receiver,
            Some((
                root.path().join("x".repeat(120)),
                acp_bridge::ConfiguredOrigins::parse("http://127.0.0.1:1", "").unwrap(),
            )),
            stop,
        )
        .await;
        pump(&mut world);
        assert!(
            !marker.exists(),
            "failed preparation must not start the child"
        );
        assert!(!world.resource::<AcpBackends>().0.contains_key("work"));
        let settled = store.run(&run.run_id).unwrap().unwrap();
        assert_eq!(
            settled.state,
            RunState::Failed,
            "an accepted Starting run must settle after proxy preparation fails"
        );
        assert_eq!(
            store.work_session("work").unwrap().unwrap().status,
            WorkStatus::Idle
        );
        assert!(
            settled
                .failure
                .unwrap()
                .message
                .contains("model proxy could not start")
        );
        assert!(store.active_run("work").unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ending_dropping_or_exiting_a_backend_closes_its_tunnels() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for mode in ["end", "drop", "exit"] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("proxy.sock");
            let exit = root.path().join("exit");
            let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = model.local_addr().unwrap();
            let configured =
                acp_bridge::ConfiguredOrigins::parse(&format!("https://{address}"), "").unwrap();
            let (events, mut receiver) = mpsc::channel(QUEUE);
            let (commands, control) = mpsc::channel(COMMANDS);
            let pid = Arc::new(AtomicU32::new(0));
            let stop = tokio_util::sync::CancellationToken::new();
            let handle = AcpHandle {
                pid: Arc::clone(&pid),
                commands,
                stop: stop.clone(),
            };
            let task = tokio::spawn(run_backend(
                BackendIdentity {
                    work_id: "work".into(),
                    initial_run_id: "run".into(),
                    pid,
                },
                command_wrap(
                    &which::which("sh").unwrap(),
                    &[
                        "-c".into(),
                        "while [ ! -f \"$1\" ]; do sleep 0.01; done".into(),
                        "acp-test".into(),
                        exit.to_str().unwrap().into(),
                    ],
                    root.path(),
                    child_env("", |name| std::env::var_os(name)),
                ),
                root.path().to_owned(),
                events,
                control,
                Some((socket.clone(), configured)),
                stop,
            ));
            let mut client = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(client) = tokio::net::UnixStream::connect(&socket).await {
                        break client;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            client
                .write_all(
                    format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut head = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(client.read_u8().await.unwrap());
                }
            })
            .await
            .unwrap();
            assert!(head.starts_with(b"HTTP/1.1 200"));
            let (mut upstream, _) = model.accept().await.unwrap();
            assert!(handle.cancel("stale-run"));
            client.write_all(b"alive").await.unwrap();
            let mut alive = [0; 5];
            upstream.read_exact(&mut alive).await.unwrap();
            assert_eq!(&alive, b"alive");
            match mode {
                "end" => {
                    handle.end();
                    drop(handle);
                }
                "drop" => drop(handle),
                "exit" => {
                    std::fs::write(&exit, "exit").unwrap();
                    tokio::time::timeout(Duration::from_secs(2), task)
                        .await
                        .unwrap()
                        .unwrap();
                    drop(handle);
                    assert!(!socket.exists());
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(2), upstream.read_u8())
                            .await
                            .unwrap()
                            .unwrap_err()
                            .kind(),
                        std::io::ErrorKind::UnexpectedEof
                    );
                    continue;
                }
                _ => unreachable!(),
            }
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
            assert!(!socket.exists());
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), upstream.read_u8())
                    .await
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            assert!(matches!(
                receiver.recv().await,
                Some(AcpEvent::Closed { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_ready_session_keeps_it_available_for_the_next_run() {
        use agent_client_protocol::v1::schema::{
            InitializeResponse, NewSessionResponse, PromptResponse,
        };
        use tokio::io::AsyncWriteExt;

        tokio::time::timeout(Duration::from_secs(3), async {
            let (client, peer) = tokio::net::UnixStream::pair().unwrap();
            let (read, mut write) = peer.into_split();
            let (started, first_prompt) = tokio::sync::oneshot::channel();
            let fixture = tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(read).lines();
                let mut started = Some(started);
                let mut pending = None;
                let mut counts = [0; 4];
                while let Some(line) = lines.next_line().await.unwrap() {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let (id, result) = match request["method"].as_str().unwrap() {
                        "initialize" => {
                            counts[0] += 1;
                            (request["id"].clone(), serde_json::to_value(InitializeResponse::new(ProtocolVersion::V1)).unwrap())
                        }
                        "session/new" => {
                            counts[1] += 1;
                            (request["id"].clone(), serde_json::to_value(NewSessionResponse::new("same-session")).unwrap())
                        }
                        "session/prompt" => {
                            counts[2] += 1;
                            assert_eq!(request["params"]["sessionId"], "same-session");
                            if let Some(started) = started.take() {
                                pending = Some(request["id"].clone());
                                started.send(()).unwrap();
                                continue;
                            }
                            (request["id"].clone(), serde_json::to_value(PromptResponse::new(StopReason::EndTurn)).unwrap())
                        }
                        "session/cancel" => {
                            counts[3] += 1;
                            (pending.take().unwrap(), serde_json::to_value(PromptResponse::new(StopReason::Cancelled)).unwrap())
                        }
                        method => panic!("unexpected ACP method {method}"),
                    };
                    let response = serde_json::json!({"jsonrpc":"2.0", "id":id, "result":result});
                    write.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                }
                counts
            });
            let (read, write) = client.into_split();
            let (commands, receiver) = mpsc::channel(COMMANDS);
            let (events, mut received) = mpsc::channel(QUEUE);
            let shared = Arc::new(SharedRun::default());
            shared.begin("first");
            let session = tokio::spawn(Client.v1().connect_with(
                ByteStreams::new(write.compat_write(), read.compat()),
                async move |cx| session_loop(cx, PathBuf::from("/tmp"), events, shared, receiver).await,
            ));
            commands.send(Command::Prompt { run_id: "first".into(), text: "wait for cancellation".into() }).await.unwrap();
            first_prompt.await.unwrap();
            assert!(matches!(received.recv().await, Some(AcpEvent::Ready { run_id }) if run_id == "first"));
            commands.send(Command::Cancel { run_id: "first".into() }).await.unwrap();
            assert!(matches!(received.recv().await, Some(AcpEvent::Finished { run_id, outcome: TurnOutcome::Stopped(_) }) if run_id == "first"));
            assert!(!session.is_finished(), "ordinary cancellation must not close a ready session");
            commands.send(Command::Prompt { run_id: "second".into(), text: "complete on the same session".into() }).await.unwrap();
            assert!(matches!(received.recv().await, Some(AcpEvent::Finished { run_id, outcome: TurnOutcome::Answered(_) }) if run_id == "second"));
            commands.send(Command::End).await.unwrap();
            session.await.unwrap().unwrap();
            assert_eq!(fixture.await.unwrap(), [1, 1, 2, 1]);
        }).await.expect("ready ACP session cancellation and reuse must finish");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_preparation_never_starts_a_child_or_proxy() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("started");
        let socket = root.path().join("proxy.sock");
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let (events, mut receiver) = mpsc::channel(QUEUE);
        let (_sender, commands) = mpsc::channel(COMMANDS);
        run_backend(
            BackendIdentity {
                work_id: "work".into(),
                initial_run_id: "accepted-run".into(),
                pid: Arc::new(AtomicU32::new(0)),
            },
            command_wrap(
                &which::which("sh").unwrap(),
                &[
                    "-c".into(),
                    "touch \"$1\"".into(),
                    "acp-test".into(),
                    marker.to_str().unwrap().into(),
                ],
                root.path(),
                child_env("", |name| std::env::var_os(name)),
            ),
            root.path().to_owned(),
            events,
            commands,
            Some((
                socket.clone(),
                acp_bridge::ConfiguredOrigins::parse("https://localhost:443", "").unwrap(),
            )),
            stop,
        )
        .await;
        assert!(!marker.exists());
        assert!(!socket.exists());
        assert!(matches!(
            receiver.recv().await,
            Some(AcpEvent::Closed {
                confirmed: true,
                ..
            })
        ));
    }

    #[test]
    fn session_end_cannot_be_blocked_by_a_full_command_queue() {
        let (commands, _receiver) = mpsc::channel(1);
        let stop = tokio_util::sync::CancellationToken::new();
        let handle = AcpHandle {
            pid: Arc::new(AtomicU32::new(0)),
            commands,
            stop: stop.clone(),
        };
        assert!(handle.prompt("run", "prompt"));
        assert!(handle.end());
        assert!(stop.is_cancelled());
    }

    #[test]
    fn isolated_policy_is_explicit_and_nonisolated_fallbacks_do_not_require_it() {
        let mut connection = AcpConnection {
            endpoint: crate::host::settings::Endpoint {
                options: r#"{"network":"isolated"}"#.into(),
                ..Default::default()
            },
            command: "agent".into(),
            args: vec!["https://unapproved.invalid".into()],
        };
        let bwrap = Path::new("bwrap");
        assert!(
            bridge(&connection, Some(bwrap))
                .unwrap_err()
                .to_string()
                .contains("model endpoint")
        );
        assert!(bridge(&connection, None).unwrap().is_none());
        connection.endpoint.options = r#"{"network":"host"}"#.into();
        assert!(bridge(&connection, Some(bwrap)).unwrap().is_none());
        connection.endpoint.options = r#"{"sandbox":"none","network":"isolated"}"#.into();
        assert!(bridge(&connection, Some(bwrap)).unwrap().is_none());
        connection.endpoint.options =
            r#"{"network":"isolated","model_endpoints":["http://localhost:8080"]}"#.into();
        assert!(bridge(&connection, Some(bwrap)).unwrap().is_some());
    }

    #[test]
    fn a_launch_uses_its_resolved_sandbox_without_changing_fallbacks() {
        let program = Path::new("/agent/bin/agent");
        let cwd = Path::new("/work");
        let bwrap = Path::new("/host/bin/bwrap");
        let missing = launch(program, &[], cwd, r#"{"network":"isolated"}"#, None, None).unwrap();
        assert_eq!(missing.program, program);
        assert_eq!(missing.isolation, Isolation::Missing);
        let partial = launch(program, &[], cwd, "", Some(bwrap), None).unwrap();
        assert_eq!(partial.program, bwrap);
        assert_eq!(partial.isolation, Isolation::Partial);
        let options = r#"{"sandbox":"none","network":"isolated"}"#;
        let plain = launch(program, &[], cwd, options, Some(bwrap), None).unwrap();
        assert_eq!(plain.program, program);
        assert_eq!(plain.isolation, Isolation::None);
        let connection = AcpConnection {
            endpoint: crate::host::settings::Endpoint {
                options: options.into(),
                ..Default::default()
            },
            command: "agent".into(),
            args: Vec::new(),
        };
        assert!(bridge(&connection, Some(bwrap)).unwrap().is_none());
    }

    #[test]
    fn an_isolated_launch_runs_the_relay_and_mounts_the_socket() {
        let bridge = Bridge {
            socket: std::path::PathBuf::from("/run/ene/proxy.sock"),
            port: acp_bridge::RELAY_PORT,
        };
        let launch = plan(
            Path::new("agent"),
            &["--stdio".to_owned()],
            Path::new("/work"),
            Some(Path::new("bwrap")),
            true,
            Some(&bridge),
        )
        .expect("an isolated launch with a bridge");
        assert_eq!(launch.isolation, Isolation::Full);
        let line = launch.args.join(" ");
        assert!(line.contains("--unshare-net"), "{line}");
        assert!(
            line.contains("--bind /run/ene/proxy.sock /run/ene/proxy.sock"),
            "{line}"
        );
        // The private /tmp shadows a socket that lives there, so this bind
        // comes after that mount.
        let tmpfs = line.find("--tmpfs /tmp").expect("a private /tmp");
        let bind = line
            .find("--bind /run/ene/proxy.sock")
            .expect("the socket bind");
        assert!(tmpfs < bind, "the socket is bound after /tmp: {line}");
        assert!(
            line.contains(
                "acp-relay --socket /run/ene/proxy.sock --listen 127.0.0.1:3128 -- agent --stdio"
            ),
            "{line}"
        );
    }

    #[test]
    fn an_isolated_launch_without_a_bridge_is_refused() {
        let error = plan(
            Path::new("agent"),
            &[],
            Path::new("/work"),
            Some(Path::new("bwrap")),
            true,
            None,
        )
        .expect_err("isolation without a bridge would leave the child no route");
        assert!(error.contains("ブリッジ"), "{error}");
    }

    #[test]
    fn a_shared_network_launch_needs_no_relay() {
        let launch = plan(
            Path::new("agent"),
            &[],
            Path::new("/work"),
            Some(Path::new("bwrap")),
            false,
            None,
        )
        .expect("a partial sandbox");
        assert_eq!(launch.isolation, Isolation::Partial);
        assert!(!launch.args.join(" ").contains("acp-relay"));
    }

    #[cfg(unix)]
    #[test]
    fn the_child_command_is_resolved_with_the_configured_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let agent = bin.join("agent");
        std::fs::write(&agent, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let options = serde_json::json!({"env": {"PATH": bin.to_str().unwrap()}}).to_string();

        assert_eq!(
            resolve_program("agent", &options, root.path()).unwrap(),
            agent
        );
        // The host PATH is not consulted when the connection configures one:
        // this command exists there, but the child would never find it.
        assert!(resolve_program("sh", &options, root.path()).is_err());
        // Without a configured PATH, the inherited host PATH is the one used.
        assert!(resolve_program("sh", r#"{"env":{}}"#, root.path()).is_ok());
    }

    #[test]
    fn the_child_environment_keeps_only_the_allowlist_and_configured_values() {
        let env = child_env(
            r#"{"env":{"PATH":"/custom/bin","ENE_ACP_TEST":"1"}}"#,
            |name| match name {
                "PATH" | "OPENAI_API_KEY" => Some(OsString::from(format!("/inherited/{name}"))),
                _ => None,
            },
        );
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| key.to_str() == Some(name))
                .map(|(_, value)| value.to_str().unwrap())
        };
        assert_eq!(value("PATH"), Some("/custom/bin"));
        assert_eq!(value("ENE_ACP_TEST"), Some("1"));
        assert_eq!(value("OPENAI_API_KEY"), None);
    }

    #[test]
    fn a_late_update_after_finish_does_not_recreate_the_run() {
        let mut world = World::new();
        world.insert_resource(AcpEvents::default());
        world.insert_resource(super::super::Runs::default());
        world.resource_mut::<super::super::Runs>().begin("run-a");
        world.resource_mut::<super::super::Runs>().forget("run-a");
        // The queue is bounded now, so a test sender must be driven by a
        // runtime.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the test sender")
            .block_on(
                world
                    .resource::<AcpEvents>()
                    .sender()
                    .send(AcpEvent::Update {
                        work_id: "w1".into(),
                        run_id: "run-a".into(),
                        text: "late progress".into(),
                    }),
            )
            .unwrap();
        pump(&mut world);
        assert!(
            !world
                .resource::<super::super::Runs>()
                .0
                .contains_key("run-a"),
            "a finished run must not be recreated"
        );
    }
}

/// Drives the production session loop against an in-process ACP agent built
/// from the SDK's own `Agent` role. The client and the agent exchange the real
/// JSON-RPC frames a child process would, so the assertions are about the host's
/// observable session behavior, not about its internal functions.
#[cfg(test)]
mod session_tests {
    use super::*;
    use agent_client_protocol::{
        Agent, Channel, on_receive_notification, on_receive_request,
        v1::schema::{
            ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
            NewSessionResponse, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
            SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallUpdate,
            ToolCallUpdateFields,
        },
    };
    use std::sync::Arc;
    use tokio::sync::{Notify, mpsc};

    const WAIT: Duration = Duration::from_secs(10);

    fn chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    fn initialize(
        responder: agent_client_protocol::v1::Responder<InitializeResponse>,
        request: InitializeRequest,
    ) -> Result<(), agent_client_protocol::Error> {
        responder.respond(InitializeResponse::new(request.protocol_version))
    }

    fn new_session(
        responder: agent_client_protocol::v1::Responder<NewSessionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        responder.respond(NewSessionResponse::new("session-1"))
    }

    async fn host(
        transport: Channel,
        commands: mpsc::Receiver<Command>,
        events: mpsc::Sender<AcpEvent>,
        shared: Arc<SharedRun>,
    ) -> Option<String> {
        connect(
            transport,
            PathBuf::from("."),
            events,
            "w1".to_owned(),
            shared,
            commands,
        )
        .await
    }

    struct Events {
        receiver: mpsc::Receiver<AcpEvent>,
        seen: Vec<AcpEvent>,
    }

    impl Events {
        fn new(receiver: mpsc::Receiver<AcpEvent>) -> Self {
            Self {
                receiver,
                seen: Vec::new(),
            }
        }

        async fn next(&mut self) -> AcpEvent {
            tokio::time::timeout(WAIT, self.receiver.recv())
                .await
                .expect("the session loop keeps reporting")
                .expect("the event channel stays open")
        }

        async fn until(&mut self, mut done: impl FnMut(&[AcpEvent]) -> bool) -> &[AcpEvent] {
            while !done(&self.seen) {
                let event = self.next().await;
                self.seen.push(event);
            }
            &self.seen
        }

        async fn started(&mut self) {
            self.until(|seen| {
                seen.iter()
                    .any(|event| matches!(event, AcpEvent::Ready { .. }))
            })
            .await;
        }

        async fn updates(&mut self, count: usize) {
            self.until(|seen| {
                seen.iter()
                    .filter(|event| matches!(event, AcpEvent::Update { .. }))
                    .count()
                    >= count
            })
            .await;
        }

        async fn finished(&mut self) -> (String, TurnOutcome) {
            self.until(|seen| {
                seen.iter()
                    .any(|event| matches!(event, AcpEvent::Finished { .. }))
            })
            .await;
            match self.seen.pop() {
                Some(AcpEvent::Finished { run_id, outcome }) => (run_id, outcome),
                _ => unreachable!("the last event is the finish the wait matched"),
            }
        }

        fn updates_text(&self) -> Vec<&str> {
            self.seen
                .iter()
                .filter_map(|event| match event {
                    AcpEvent::Update { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn readies(&self) -> usize {
            self.seen
                .iter()
                .filter(|event| matches!(event, AcpEvent::Ready { .. }))
                .count()
        }
    }

    fn commands() -> (mpsc::Sender<Command>, mpsc::Receiver<Command>) {
        mpsc::channel(8)
    }

    async fn prompt(commands: &mpsc::Sender<Command>, run_id: &str, text: &str) {
        commands
            .send(Command::Prompt {
                run_id: run_id.to_owned(),
                text: text.to_owned(),
            })
            .await
            .expect("the session loop accepts a prompt");
    }

    async fn stop(
        host: tokio::task::JoinHandle<Option<String>>,
        agent: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
        commands: &mpsc::Sender<Command>,
    ) {
        commands.send(Command::End).await.expect("end is accepted");
        let ended = tokio::time::timeout(WAIT, host)
            .await
            .expect("the host closes the session")
            .expect("the host task did not panic");
        assert_eq!(
            ended, None,
            "a transport-less end is not a failure: {ended:?}"
        );
        agent.abort();
    }

    /// A prompt sent while the connection is still initializing must survive:
    /// it is queued before `session/new` answers, announced once, and settled
    /// from the final message. The agent holds its answer until the client has
    /// observed the streamed updates, so the order asserted here is the order
    /// the wire delivers.
    #[tokio::test]
    async fn a_prompt_buffered_during_initialization_runs_and_settles() {
        let (client, transport) = Channel::duplex();
        let proceed = Arc::new(Notify::new());
        let gate = Arc::clone(&proceed);
        let agent = tokio::spawn(
            Agent
                .v1()
                .name("fixture-agent")
                .on_receive_request(
                    async |request: InitializeRequest, responder, _cx| {
                        initialize(responder, request)
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: NewSessionRequest, responder, _cx| new_session(responder),
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, cx| {
                        let session = request.session_id.clone();
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::AgentThoughtChunk(chunk("thinking")),
                        ))?;
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::AgentMessageChunk(chunk("   ")),
                        ))?;
                        cx.send_notification(SessionNotification::new(
                            session,
                            SessionUpdate::AgentMessageChunk(chunk("hello")),
                        ))?;
                        gate.notified().await;
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    },
                    on_receive_request!(),
                )
                .connect_with(transport, async |cx| {
                    cx.terminated().await;
                    Ok(())
                }),
        );
        let (sender, receiver) = commands();
        prompt(&sender, "run-1", "do it").await;
        let (events, receiver_events) = mpsc::channel(16);
        let shared = Arc::new(SharedRun::default());
        shared.begin("run-1");
        let host = tokio::spawn(host(client, receiver, events, Arc::clone(&shared)));

        let mut events = Events::new(receiver_events);
        events.started().await;
        events.updates(2).await;
        assert_eq!(
            events.updates_text(),
            ["thinking", "hello"],
            "empty chunks are not forwarded, thought chunks are"
        );
        proceed.notify_one();
        let (run_id, outcome) = events.finished().await;
        assert_eq!(run_id, "run-1");
        assert_eq!(events.readies(), 1, "the run is announced once");
        let TurnOutcome::Answered(message) = outcome else {
            panic!("an ending prompt settles as answered");
        };
        assert_eq!(
            super::super::message_text(&message).as_deref(),
            Some("hello"),
            "only message text forms the final report"
        );
        stop(host, agent, &sender).await;
    }

    /// One connection serves one session and many runs: the second prompt must
    /// reach the same agent without a new ready, which is what the host claims
    /// when it keeps the process alive between instructions.
    #[tokio::test]
    async fn a_second_run_reuses_the_open_session() {
        let (client, transport) = Channel::duplex();
        let proceed = Arc::new(Notify::new());
        let gate = Arc::clone(&proceed);
        let agent = tokio::spawn(
            Agent
                .v1()
                .name("fixture-agent")
                .on_receive_request(
                    async |request: InitializeRequest, responder, _cx| {
                        initialize(responder, request)
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: NewSessionRequest, responder, _cx| new_session(responder),
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, cx| {
                        let text = match request.prompt.first() {
                            Some(ContentBlock::Text(text)) => text.text.clone(),
                            _ => String::new(),
                        };
                        cx.send_notification(SessionNotification::new(
                            request.session_id,
                            SessionUpdate::AgentMessageChunk(chunk(&text)),
                        ))?;
                        gate.notified().await;
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    },
                    on_receive_request!(),
                )
                .connect_with(transport, async |cx| {
                    cx.terminated().await;
                    Ok(())
                }),
        );
        let (sender, receiver) = commands();
        let (events, receiver_events) = mpsc::channel(16);
        let shared = Arc::new(SharedRun::default());
        shared.begin("run-1");
        let host = tokio::spawn(host(client, receiver, events, Arc::clone(&shared)));

        let mut events = Events::new(receiver_events);
        prompt(&sender, "run-1", "first").await;
        events.updates(1).await;
        proceed.notify_one();
        let (first, _) = events.finished().await;
        prompt(&sender, "run-2", "second").await;
        events.updates(2).await;
        proceed.notify_one();
        let (second, _) = events.finished().await;
        assert_eq!((first.as_str(), second.as_str()), ("run-1", "run-2"));
        assert_eq!(events.readies(), 1, "the session opens once");
        assert_eq!(events.updates_text(), ["first", "second"]);
        stop(host, agent, &sender).await;
    }

    /// Alpha has no approval UI. An ACP permission request must be answered
    /// with an explicit refusal: leaving it unanswered would hang the agent,
    /// and approving it would grant an operation the user never saw.
    #[tokio::test]
    async fn a_permission_request_is_refused() {
        let (client, transport) = Channel::duplex();
        let (observed, mut outcomes) = mpsc::unbounded_channel();
        let agent = tokio::spawn(
            Agent
                .v1()
                .name("fixture-agent")
                .on_receive_request(
                    async |request: InitializeRequest, responder, _cx| {
                        initialize(responder, request)
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: NewSessionRequest, responder, _cx| new_session(responder),
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, cx| {
                        let session = request.session_id;
                        let observed = observed.clone();
                        let connection = cx.clone();
                        // Asking from inside the handler would block the
                        // connection driver, so the request runs beside it.
                        cx.spawn(async move {
                            let response = connection
                                .send_request(RequestPermissionRequest::new(
                                    session,
                                    ToolCallUpdate::new("call-1", ToolCallUpdateFields::new()),
                                    Vec::new(),
                                ))
                                .block_task()
                                .await?;
                            let _ = observed.send(response.outcome);
                            responder.respond(PromptResponse::new(StopReason::EndTurn))
                        })?;
                        Ok(())
                    },
                    on_receive_request!(),
                )
                .connect_with(transport, async |cx| {
                    cx.terminated().await;
                    Ok(())
                }),
        );
        let (sender, receiver) = commands();
        let (events, receiver_events) = mpsc::channel(16);
        let shared = Arc::new(SharedRun::default());
        shared.begin("run-1");
        let host = tokio::spawn(host(client, receiver, events, Arc::clone(&shared)));

        let mut events = Events::new(receiver_events);
        prompt(&sender, "run-1", "touch a file").await;
        let outcome = tokio::time::timeout(WAIT, outcomes.recv())
            .await
            .expect("the agent receives an answer")
            .expect("the agent reports the outcome");
        assert!(
            matches!(outcome, RequestPermissionOutcome::Cancelled),
            "an unseen permission request is refused, not approved"
        );
        let (_, outcome) = events.finished().await;
        assert!(matches!(outcome, TurnOutcome::Answered(_)));
        stop(host, agent, &sender).await;
    }

    /// Cancelling a run in flight must reach the agent as `session/cancel` and
    /// settle as stopped, not as a failure.
    #[tokio::test]
    async fn cancelling_an_inflight_run_asks_the_agent_to_stop() {
        let (client, transport) = Channel::duplex();
        let cancelled = Arc::new(Notify::new());
        let notified = Arc::clone(&cancelled);
        let waiting = Arc::clone(&cancelled);
        let agent = tokio::spawn(
            Agent
                .v1()
                .name("fixture-agent")
                .on_receive_request(
                    async |request: InitializeRequest, responder, _cx| {
                        initialize(responder, request)
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: NewSessionRequest, responder, _cx| new_session(responder),
                    on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_notification: CancelNotification, _cx| {
                        notified.notify_one();
                        Ok(())
                    },
                    on_receive_notification!(),
                )
                .on_receive_request(
                    async move |_request: PromptRequest, responder, cx| {
                        // A handler that awaited here would hold the incoming
                        // actor, and the cancel notification could never arrive.
                        let waiting = waiting.clone();
                        cx.spawn(async move {
                            waiting.notified().await;
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        })?;
                        Ok(())
                    },
                    on_receive_request!(),
                )
                .connect_with(transport, async |cx| {
                    cx.terminated().await;
                    Ok(())
                }),
        );
        let (sender, receiver) = commands();
        let (events, receiver_events) = mpsc::channel(16);
        let shared = Arc::new(SharedRun::default());
        shared.begin("run-1");
        let host = tokio::spawn(host(client, receiver, events, Arc::clone(&shared)));

        let mut events = Events::new(receiver_events);
        prompt(&sender, "run-1", "long task").await;
        events.started().await;
        sender
            .send(Command::Cancel {
                run_id: "run-1".into(),
            })
            .await
            .expect("a cancel is accepted");
        let (run_id, outcome) = events.finished().await;
        assert_eq!(run_id, "run-1");
        assert!(
            matches!(outcome, TurnOutcome::Stopped(_)),
            "a cancelled prompt settles as stopped"
        );
        stop(host, agent, &sender).await;
    }

    /// A prompt the agent answers with an error must fail the run rather than
    /// be reported as an empty answer.
    #[tokio::test]
    async fn a_prompt_error_fails_the_run() {
        let (client, transport) = Channel::duplex();
        let agent = tokio::spawn(
            Agent
                .v1()
                .name("fixture-agent")
                .on_receive_request(
                    async |request: InitializeRequest, responder, _cx| {
                        initialize(responder, request)
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: NewSessionRequest, responder, _cx| new_session(responder),
                    on_receive_request!(),
                )
                .on_receive_request(
                    async |_request: PromptRequest, responder, _cx| {
                        responder.respond_with_error(agent_client_protocol::Error::internal_error())
                    },
                    on_receive_request!(),
                )
                .connect_with(transport, async |cx| {
                    cx.terminated().await;
                    Ok(())
                }),
        );
        let (sender, receiver) = commands();
        let (events, receiver_events) = mpsc::channel(16);
        let shared = Arc::new(SharedRun::default());
        shared.begin("run-1");
        let host = tokio::spawn(host(client, receiver, events, Arc::clone(&shared)));

        let mut events = Events::new(receiver_events);
        prompt(&sender, "run-1", "fail").await;
        let (_, outcome) = events.finished().await;
        assert!(
            matches!(outcome, TurnOutcome::Failed(_)),
            "an agent error is not an empty answer"
        );
        stop(host, agent, &sender).await;
    }
}
