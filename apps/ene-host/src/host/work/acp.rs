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

/// Longest a forced kill may take to be reaped; `work::FORCE_GRACE` is the
/// wait that must outlast it before a stop counts as unconfirmed.
/// How many ACP events may queue before a backend waits. A child that
/// floods notifications is slowed instead of grown.
const QUEUE: usize = 256;

/// How many commands may wait for the backend before a control call reports
/// that it could not be delivered.
const COMMANDS: usize = 64;

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
    /// The HTTP proxy of a network-isolated launch, closed with the session.
    proxy: Option<acp_bridge::Proxy>,
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
        if let Some(proxy) = &self.proxy {
            proxy.close();
        }
        self.commands.try_send(Command::End).is_ok()
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
    connection: &AcpConnection,
) -> Result<AcpHandle, HostError> {
    let command = connection.command.trim();
    if command.is_empty() {
        return Err(HostError::invalid("the ACP connection has no command"));
    }
    let cwd = std::env::current_dir()
        .map_err(|error| HostError::unavailable(format!("ACP working directory: {error}")))?;
    // Resolve against the PATH the child will actually receive, not the
    // host's: a configured `env.PATH` replaces the inherited one.
    let program = resolve_program(command, &connection.endpoint.options, &cwd)?;
    let (bridge, proxy) = bridge(world, &connection.endpoint.options);
    let Launch {
        program,
        args,
        isolation,
    } = launch(
        &program,
        &connection.args,
        &cwd,
        &connection.endpoint.options,
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
    let handle = AcpHandle {
        pid: Arc::clone(&pid),
        commands,
        proxy,
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
        return Err(HostError::unavailable("the host runtime is unavailable"));
    };
    runtime.spawn(run_backend(
        work_id.to_owned(),
        command_wrap(&program, &args, &cwd, env),
        cwd,
        events,
        pid,
        receiver,
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

fn closed(work_id: String, error: String) -> AcpEvent {
    AcpEvent::Closed {
        work_id,
        run_id: None,
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

async fn run_backend(
    work_id: String,
    mut wrap: CommandWrap,
    cwd: PathBuf,
    events: mpsc::Sender<AcpEvent>,
    pid: Arc<AtomicU32>,
    commands: mpsc::Receiver<Command>,
) {
    let mut child = match wrap.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = events
                .send(closed(
                    work_id,
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
        let _ = events
            .send(closed(
                work_id,
                "the ACP agent has no standard input/output".to_owned(),
            ))
            .await;
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
        .await;
    let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
    let _ = events
        .send(AcpEvent::Closed {
            work_id,
            run_id: shared.run_id(),
            error: result.err().map(|error| error.to_string()),
            confirmed,
            pids,
        })
        .await;
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

/// Runs `InitializeRequest` and `NewSessionRequest` while watching the command
/// channel. A handshake cannot be cancelled through ACP, so a stop request
/// aborts the connection and the caller force-kills the child. The prompt that
/// arrived meanwhile is returned to be served after the session exists.
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
                Some(Command::Cancel { .. } | Command::End) | None => {
                    if let Some((run_id, _)) = &first {
                        shared.begin(run_id);
                    }
                    return Ok(None);
                }
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
                Some(Command::Cancel { .. } | Command::End) | None => {
                    if let Some((run_id, _)) = &first {
                        shared.begin(run_id);
                    }
                    return Ok(None);
                }
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

async fn terminate(child: &mut Box<dyn ChildWrapper>, timeout: Duration) -> (bool, Vec<u32>) {
    let pids: Vec<u32> = child.id().into_iter().collect();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return (true, pids);
    }
    // `start_kill` signals the whole process group; `wait` reaps it.
    if child.start_kill().is_err() {
        return (matches!(child.try_wait(), Ok(Some(_))), pids);
    }
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    (matches!(waited, Ok(Ok(_))), pids)
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
        HostError::unavailable(format!(
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
fn bridge(world: &World, options: &str) -> (Option<Bridge>, Option<acp_bridge::Proxy>) {
    if !network_isolated(options).unwrap_or(false) || which::which("bwrap").is_err() {
        return (None, None);
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) else {
        return (None, None);
    };
    // Under the system temp directory: a unix socket path has to stay short
    // enough to bind, and the runtime directory can be deep.
    let socket = std::env::temp_dir().join(format!(
        "ene-acp-{}.sock",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    match acp_bridge::open(&runtime, socket) {
        Ok(proxy) => (Some(proxy.bridge.clone()), Some(proxy)),
        Err(error) => {
            tracing::warn!(%error, "the ACP proxy did not start");
            (None, None)
        }
    }
}

fn launch(
    program: &Path,
    args: &[String],
    cwd: &Path,
    options: &str,
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
        which::which("bwrap").ok().as_deref(),
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
