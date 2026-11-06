//! Minimal ACP v1 client over an explicit stdio subprocess.
//!
//! The process is configured by the host (never a shell), receives only a
//! filtered environment, and advertises `fs` read/write with `terminal` off
//! unless the binding opts in. The official ACP SDK owns JSON-RPC framing,
//! request ids and response routing; this module owns the process lifecycle,
//! terminal registry, workspace boundary and the host's turn state.
use crate::{
    domain::{Error, ErrorKind},
    infrastructure::workspace::Workspace,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, Error as AcpError, ErrorCode, Raw, Responder as RawResponder, V1,
    schema::ProtocolVersion,
    util::MatchDispatch,
    v1::{ActiveSession, ConnectionTo, Responder, SessionMessage, schema},
};
use ene_plugin::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome, SessionUpdate,
    StopReason, ToolCallUpdate,
};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const MAX_UPDATES: usize = 4096;
const STDERR_BYTES: usize = 4096;
const TERMINAL_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub(crate) struct AcpConfig {
    /// Executable followed by arguments; never interpreted by a shell.
    pub(crate) command: Vec<String>,
    pub(crate) workspace: Workspace,
    pub(crate) cwd: PathBuf,
    /// Terminal capability; `None` advertises and accepts no terminal requests.
    pub(crate) terminal: Option<TerminalLimits>,
}

/// Host-enforced limits for the explicit terminal opt-in.
#[derive(Clone, Debug)]
pub(crate) struct TerminalLimits {
    pub(crate) allowed: Vec<String>,
    /// Additional environment names the terminal request may set. Anything
    /// else is refused, so a command cannot inherit an unlisted interpreter
    /// hook such as `PYTHONSTARTUP` or `NODE_OPTIONS`.
    pub(crate) allowed_env: Vec<String>,
    pub(crate) max_seconds: u64,
    pub(crate) max_output: usize,
}

struct Terminal {
    commands: mpsc::Sender<()>,
    output: Arc<Mutex<TerminalOutput>>,
    exit: watch::Receiver<Option<Option<i32>>>,
}

#[derive(Default)]
struct TerminalOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

struct Permission {
    request_id: String,
    options: Vec<PermissionOption>,
    responder: Responder<schema::RequestPermissionResponse>,
}

#[derive(Clone)]
struct ClientContext {
    shared: Arc<Mutex<State>>,
    workspace: Workspace,
    terminal: Option<Arc<TerminalLimits>>,
    terminals: Arc<Mutex<HashMap<String, Terminal>>>,
}

/// How a prompt turn ended: the agent awaits an answer, or it stopped.
#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub(crate) state: TurnState,
}

#[derive(Clone, Debug)]
pub(crate) enum TurnState {
    Waiting(Box<Waiting>),
    Stopped(StopReason),
}

#[derive(Clone, Debug)]
pub(crate) struct Waiting {
    pub(crate) request_id: String,
    pub(crate) tool_call: ToolCallUpdate,
    pub(crate) options: Vec<PermissionOption>,
}

/// What the host keeps between prompts: one answer the agent is waiting for,
/// the one-shot terminal approval, and whether this agent is usable.
struct State {
    /// Increments for every prompt and answer, so an approval never outlives
    /// the turn it was given in.
    turn: u64,
    /// Updates this turn has delivered or is holding, whether or not the host
    /// is reading them right now.
    updates: usize,
    waiting: Option<Waiting>,
    permission: Option<Permission>,
    approved: Option<u64>,
    failed: Option<&'static str>,
}

impl State {
    fn new() -> Self {
        Self {
            turn: 0,
            updates: 0,
            waiting: None,
            permission: None,
            approved: None,
            failed: None,
        }
    }
}

pub(crate) struct AcpSession {
    child: Box<dyn ChildWrapper>,
    terminals: Arc<Mutex<HashMap<String, Terminal>>>,
    context: ClientContext,
    connection: ConnectionTo<Agent>,
    /// The one session this process serves; its routing lives until it drops.
    session: Option<ActiveSession<'static, Agent>>,
    session_id: schema::SessionId,
    /// Updates the agent sent before the host started reading them again
    /// (while it answers a permission request). Bounded and counted before
    /// the SDK's unbounded session queue sees them.
    backlog: mpsc::Receiver<SessionUpdate>,
    driver: JoinHandle<()>,
    shared: Arc<Mutex<State>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    shutdown: CancellationToken,
}

impl AcpSession {
    pub(crate) async fn start(config: AcpConfig) -> Result<Self, Error> {
        if config.command.is_empty()
            || config.command.len() > 64
            || config.command.iter().any(|part| {
                part.is_empty() || part.len() > 4096 || part.chars().any(char::is_control)
            })
            || !config.cwd.is_dir()
        {
            return Err(Error::invalid());
        }
        let mut command = Command::new(&config.command[0]);
        command
            .args(&config.command[1..])
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for key in BASE_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut wrapped = grouped(command);
        let mut child = wrapped.spawn().map_err(|error| Error {
            kind: ErrorKind::ExternalFailure,
            reason: Some(if error.kind() == std::io::ErrorKind::NotFound {
                "acp_command_not_found"
            } else {
                "acp_spawn_failed"
            }),
        })?;
        let stdin = child.stdin().take().ok_or_else(Error::unavailable)?;
        let stdout = child.stdout().take().ok_or_else(Error::unavailable)?;
        let stderr = child.stderr().take().ok_or_else(Error::unavailable)?;

        let shared = Arc::new(Mutex::new(State::new()));
        let terminals = Arc::new(Mutex::new(HashMap::<String, Terminal>::new()));
        let terminal = config.terminal.map(Arc::new);
        let terminal_enabled = terminal.is_some();
        let context = ClientContext {
            shared: shared.clone(),
            workspace: config.workspace,
            terminal,
            terminals: terminals.clone(),
        };
        let captured = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, captured.clone()));
        let shutdown = CancellationToken::new();
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let (ready_tx, ready_rx) = oneshot::channel::<
            Result<
                (
                    ActiveSession<'static, Agent>,
                    schema::SessionId,
                    ConnectionTo<Agent>,
                ),
                AcpError,
            >,
        >();
        let cwd = config.cwd;
        // Count and bound updates here, before the SDK's session route queues
        // them. Its queue is unbounded and keeps growing while the host is
        // busy answering a permission request.
        let (backlog_tx, backlog) = mpsc::channel::<SessionUpdate>(MAX_UPDATES);
        let backlog_tx = Arc::new(StdMutex::new(Some(backlog_tx)));
        let recorded = {
            let shared = shared.clone();
            let backlog_tx = backlog_tx.clone();
            move |notification: schema::SessionNotification, _: ConnectionTo<Agent>| {
                let shared = shared.clone();
                let backlog_tx = backlog_tx.clone();
                async move {
                    let Some(update) = accepted(notification) else {
                        return Ok(());
                    };
                    let mut state = shared.lock().await;
                    if state.failed.is_some() {
                        return Ok(());
                    }
                    state.updates = state.updates.saturating_add(1);
                    let mut backlog = backlog_tx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let full = match backlog.as_ref() {
                        Some(sender) => sender.try_send(update).is_err(),
                        None => true,
                    };
                    if state.updates > MAX_UPDATES || full {
                        state.failed = Some("acp_update_limit");
                        *backlog = None;
                    }
                    Ok(())
                }
            }
        };

        let driver_shared = shared.clone();
        let driver_stderr = captured.clone();
        let driver_shutdown = shutdown.clone();
        let session_shutdown = driver_shutdown.clone();
        let driver = tokio::spawn(async move {
            let result = Client
                .v1()
                .name("ene")
                .on_receive_notification(
                    recorded,
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let initialized = timeout(
                        REQUEST_TIMEOUT,
                        cx.send_request(initialize_request(terminal_enabled))
                            .block_task(),
                    )
                    .await
                    .map_err(|_| AcpError::request_cancelled())??;
                    let _ = initialized;
                    // The SDK routes this session's messages, and publishes
                    // the request only after the route is installed, so no
                    // update can arrive unrouted.
                    let setup = timeout(
                        REQUEST_TIMEOUT,
                        cx.build_session(cwd).block_task().start_session(),
                    )
                    .await
                    .map_err(|_| AcpError::request_cancelled())??;
                    let session_id = setup.session.session_id().clone();
                    if session_shutdown.is_cancelled() {
                        return Ok(());
                    }
                    let _ = ready_tx.send(Ok((setup.session, session_id, cx.clone())));
                    session_shutdown.cancelled().await;
                    Ok(())
                })
                .await;
            if !driver_shutdown.is_cancelled() {
                let reason = result
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "agent_exited".into());
                fail(&driver_shared, &driver_stderr, reason).await;
            }
        });

        match timeout(REQUEST_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok((session, session_id, connection)))) => Ok(Self {
                child,
                terminals,
                context,
                connection,
                session: Some(session),
                session_id,
                backlog,
                driver,
                shared,
                stderr: captured,
                stderr_task,
                shutdown,
            }),
            _ => {
                shutdown.cancel();
                driver.abort();
                stderr_task.abort();
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(2), child.wait()).await;
                Err(Error {
                    kind: ErrorKind::ExternalFailure,
                    reason: Some("acp_agent_failed"),
                })
            }
        }
    }

    /// Starts a prompt turn. Confirmed updates go to `updates` as they arrive,
    /// so the caller records them while the turn runs.
    pub(crate) async fn prompt(
        &mut self,
        prompt: &[ContentBlock],
        updates: mpsc::Sender<SessionUpdate>,
    ) -> Result<Turn, Error> {
        self.begin_turn().await?;
        let mut session = self.session.take().ok_or_else(conflict)?;
        let result = match session.send_prompt_blocks(prompt.to_vec()) {
            Ok(()) => self.read_turn(&mut session, &updates).await,
            Err(_) => Err(Error::new(ErrorKind::Unavailable)),
        };
        self.session = Some(session);
        result
    }

    /// Answers the agent's pending permission request and reads the rest of
    /// the turn.
    pub(crate) async fn respond(
        &mut self,
        request_id: &str,
        outcome: &RequestPermissionOutcome,
        updates: mpsc::Sender<SessionUpdate>,
    ) -> Result<Turn, Error> {
        let permission = {
            let mut state = self.shared.lock().await;
            let Some(permission) = state.permission.take() else {
                return Err(conflict());
            };
            if permission.request_id != request_id {
                state.permission = Some(permission);
                return Err(conflict());
            }
            // A terminal command is only accepted immediately after the user
            // selected an allow option for the agent's permission request.
            if let RequestPermissionOutcome::Selected(selected) = outcome
                && permission.options.iter().any(|option| {
                    option.option_id == selected.option_id
                        && matches!(
                            option.kind,
                            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                        )
                })
            {
                state.approved = Some(state.turn);
            }
            state.waiting = None;
            permission
        };
        if permission
            .responder
            .respond(schema::RequestPermissionResponse::new(outcome.clone()))
            .is_err()
        {
            return Err(Error::unavailable());
        }
        let mut session = self.session.take().ok_or_else(conflict)?;
        let result = self.read_turn(&mut session, &updates).await;
        self.session = Some(session);
        result
    }

    pub(crate) async fn kill(mut self) {
        self.shutdown.cancel();
        self.session = None;
        self.driver.abort();
        self.stderr_task.abort();
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
        let terminals = self.terminals.lock().await;
        for terminal in terminals.values() {
            let _ = terminal.commands.try_send(());
        }
    }

    async fn begin_turn(&self) -> Result<(), Error> {
        let mut state = self.shared.lock().await;
        if state.failed.is_some() {
            return Err(Error::new(ErrorKind::ExternalFailure));
        }
        if state.permission.is_some() {
            return Err(conflict());
        }
        state.turn += 1;
        state.updates = 0;
        state.approved = None;
        state.waiting = None;
        Ok(())
    }

    async fn read_turn(
        &mut self,
        session: &mut ActiveSession<'static, Agent>,
        updates: &mpsc::Sender<SessionUpdate>,
    ) -> Result<Turn, Error> {
        let result = timeout(
            TURN_TIMEOUT,
            read_until_end(
                &self.context,
                &self.connection,
                session,
                &mut self.backlog,
                updates,
            ),
        )
        .await;
        let reason = match result {
            Ok(Ok(turn)) => {
                // A flood can race the prompt response. The handler marks the
                // session failed even when the response already won.
                match self.shared.lock().await.failed {
                    Some(reason) => reason.to_owned(),
                    None => return Ok(turn),
                }
            }
            Ok(Err(reason)) => reason,
            Err(_) => {
                // The agent is still working on a turn nobody waits for.
                let _ = self
                    .connection
                    .send_notification(schema::CancelNotification::new(self.session_id.clone()));
                self.shared.lock().await.waiting = None;
                return Err(Error::new(ErrorKind::Unavailable));
            }
        };
        self.shared
            .lock()
            .await
            .failed
            .get_or_insert("acp_agent_failed");
        let captured = self.stderr.lock().await;
        eprintln!(
            "ene-host acp: {reason}: {}",
            String::from_utf8_lossy(&captured)
        );
        Err(Error {
            kind: ErrorKind::ExternalFailure,
            reason: Some("acp_agent_failed"),
        })
    }
}

fn conflict() -> Error {
    Error::new(ErrorKind::StateConflict)
}

/// Delivers updates the capture handler already took out of the SDK queue.
/// The SDK dispatches in order, so a prompt response or permission request
/// implies every update sent before it is already queued here.
fn forward_queued(
    backlog: &mut mpsc::Receiver<SessionUpdate>,
    updates: &mpsc::Sender<SessionUpdate>,
) -> std::result::Result<(), String> {
    while let Ok(update) = backlog.try_recv() {
        if updates.try_send(update).is_err() {
            return Err("update backlog".into());
        }
    }
    Ok(())
}

/// One turn's messages: the updates the journal records, the request answers
/// a session-scoped message needs, and where the turn ends.
async fn read_until_end(
    context: &ClientContext,
    connection: &ConnectionTo<Agent>,
    session: &mut ActiveSession<'static, Agent>,
    backlog: &mut mpsc::Receiver<SessionUpdate>,
    updates: &mpsc::Sender<SessionUpdate>,
) -> std::result::Result<Turn, String> {
    loop {
        let dispatch = tokio::select! {
            biased;
            message = session.read_update() => match message.map_err(|error| error.to_string())? {
                SessionMessage::PromptResponse(result) => {
                    forward_queued(backlog, updates)?;
                    let response = result.map_err(|error| error.to_string())?;
                    return Ok(Turn {
                        state: TurnState::Stopped(response.stop_reason),
                    });
                }
                SessionMessage::Message(dispatch) => dispatch,
                _ => continue,
            },
            // Updates were taken out of the SDK's queue before it could grow.
            update = backlog.recv() => match update {
                Some(update) => {
                    if updates.try_send(update).is_err() {
                        return Err("update backlog".into());
                    }
                    continue;
                }
                // The capture handler closed the backlog after a flood.
                None => return Err("update limit".into()),
            },
        };
        let permission = {
            let context = context.clone();
            async move |request: schema::RequestPermissionRequest,
                        responder: RawResponder<schema::RequestPermissionResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_permission(&context, request, responder).await?;
                Ok(())
            }
        };
        let read_file = {
            let (context, connection) = (context.clone(), connection.clone());
            async move |request: schema::ReadTextFileRequest,
                        responder: RawResponder<schema::ReadTextFileResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_read_file(&context, request, responder, connection.clone()).await?;
                Ok(())
            }
        };
        let write_file = {
            let (context, connection) = (context.clone(), connection.clone());
            async move |request: schema::WriteTextFileRequest,
                        responder: RawResponder<schema::WriteTextFileResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_write_file(&context, request, responder, connection.clone()).await?;
                Ok(())
            }
        };
        let create_terminal = {
            let context = context.clone();
            async move |request: schema::CreateTerminalRequest,
                        responder: RawResponder<schema::CreateTerminalResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_create_terminal(&context, request, responder).await?;
                Ok(())
            }
        };
        let terminal_output = {
            let context = context.clone();
            async move |request: schema::TerminalOutputRequest,
                        responder: RawResponder<schema::TerminalOutputResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_terminal_output(&context, request, responder).await?;
                Ok(())
            }
        };
        let wait_for_exit = {
            let (context, connection) = (context.clone(), connection.clone());
            async move |request: schema::WaitForTerminalExitRequest,
                        responder: RawResponder<schema::WaitForTerminalExitResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_wait_for_exit(&context, request, responder, connection.clone()).await?;
                Ok(())
            }
        };
        let kill_terminal = {
            let context = context.clone();
            async move |request: schema::KillTerminalRequest,
                        responder: RawResponder<schema::KillTerminalResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_kill_terminal(&context, request, responder).await?;
                Ok(())
            }
        };
        let release_terminal = {
            let context = context.clone();
            async move |request: schema::ReleaseTerminalRequest,
                        responder: RawResponder<schema::ReleaseTerminalResponse, Raw>| {
                let responder = responder
                    .into_protocol::<V1>()
                    .map_err(|_| AcpError::invalid_request())?;
                on_release_terminal(&context, request, responder).await?;
                Ok(())
            }
        };
        MatchDispatch::new(dispatch.into_raw())
            .if_request(permission)
            .await
            .if_request(read_file)
            .await
            .if_request(write_file)
            .await
            .if_request(create_terminal)
            .await
            .if_request(terminal_output)
            .await
            .if_request(wait_for_exit)
            .await
            .if_request(kill_terminal)
            .await
            .if_request(release_terminal)
            .await
            .otherwise_ignore()
            .map_err(|error| error.to_string())?;
        if let Some(waiting) = context.shared.lock().await.waiting.take() {
            forward_queued(backlog, updates)?;
            return Ok(Turn {
                state: TurnState::Waiting(Box::new(waiting)),
            });
        }
    }
}

fn grouped(command: Command) -> CommandWrap {
    let mut wrapped = CommandWrap::from(command);
    #[cfg(unix)]
    wrapped.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(JobObject);
    wrapped.wrap(KillOnDrop);
    wrapped
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if let Ok(terminals) = self.terminals.try_lock() {
            for terminal in terminals.values() {
                let _ = terminal.commands.try_send(());
            }
        }
    }
}

fn initialize_request(terminal: bool) -> schema::InitializeRequest {
    schema::InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(
            schema::ClientCapabilities::new()
                .fs(schema::FileSystemCapabilities::new()
                    .read_text_file(true)
                    .write_text_file(true))
                .terminal(terminal),
        )
        .client_info(schema::Implementation::new(
            "ene",
            env!("CARGO_PKG_VERSION"),
        ))
}

/// Marks the agent unusable: nobody will answer a turn it still runs.
async fn fail(shared: &Arc<Mutex<State>>, stderr: &Arc<Mutex<Vec<u8>>>, reason: String) {
    let mut state = shared.lock().await;
    state.failed = Some("acp_agent_failed");
    state.waiting = None;
    if let Some(permission) = state.permission.take() {
        let _ = permission
            .responder
            .respond(schema::RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
    }
    let captured = stderr.lock().await;
    eprintln!(
        "ene-host acp: {reason}: {}",
        String::from_utf8_lossy(&captured)
    );
}

/// The session update the journal records, when this host knows its kind.
fn accepted(notification: schema::SessionNotification) -> Option<SessionUpdate> {
    match notification.update {
        SessionUpdate::AgentMessageChunk(_)
        | SessionUpdate::AgentThoughtChunk(_)
        | SessionUpdate::ToolCall(_)
        | SessionUpdate::ToolCallUpdate(_)
        | SessionUpdate::Plan(_) => Some(notification.update),
        _ => None,
    }
}

async fn on_permission(
    context: &ClientContext,
    request: schema::RequestPermissionRequest,
    responder: Responder<schema::RequestPermissionResponse>,
) -> Result<(), AcpError> {
    let mut state = context.shared.lock().await;
    if state.permission.is_some() {
        let _ = responder.respond(schema::RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
        return Ok(());
    }
    // A new permission request invalidates a previous, unused terminal
    // approval even within the same turn.
    state.approved = None;
    let request_id = uuid::Uuid::new_v4().to_string();
    state.waiting = Some(Waiting {
        request_id: request_id.clone(),
        tool_call: request.tool_call,
        options: request.options.clone(),
    });
    state.permission = Some(Permission {
        request_id,
        options: request.options,
        responder,
    });
    Ok(())
}

async fn on_read_file(
    context: &ClientContext,
    request: schema::ReadTextFileRequest,
    responder: Responder<schema::ReadTextFileResponse>,
    cx: ConnectionTo<Agent>,
) -> Result<(), AcpError> {
    let workspace = context.workspace.clone();
    let path = request.path;
    let line = request.line;
    let limit = request.limit;
    let _ = cx.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            relative_to(&workspace, &path).and_then(|path| workspace.read(&path, line, limit))
        })
        .await;
        match result {
            Ok(Ok(read)) => {
                let _ = responder.respond(schema::ReadTextFileResponse::new(read.content));
            }
            Ok(Err(error)) => {
                let _ = responder.respond_with_error(acp_error(error));
            }
            Err(_) => {
                let _ = responder.respond_with_error(AcpError::internal_error());
            }
        }
        Ok(())
    });
    Ok(())
}

async fn on_write_file(
    context: &ClientContext,
    request: schema::WriteTextFileRequest,
    responder: Responder<schema::WriteTextFileResponse>,
    cx: ConnectionTo<Agent>,
) -> Result<(), AcpError> {
    let workspace = context.workspace.clone();
    let path = request.path;
    let content = request.content;
    let _ = cx.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            relative_to(&workspace, &path).and_then(|path| workspace.write(&path, &content))
        })
        .await;
        match result {
            Ok(Ok(())) => {
                let _ = responder.respond(schema::WriteTextFileResponse::new());
            }
            Ok(Err(error)) => {
                let _ = responder.respond_with_error(acp_error(error));
            }
            Err(_) => {
                let _ = responder.respond_with_error(AcpError::internal_error());
            }
        }
        Ok(())
    });
    Ok(())
}

async fn on_create_terminal(
    context: &ClientContext,
    request: schema::CreateTerminalRequest,
    responder: Responder<schema::CreateTerminalResponse>,
) -> Result<(), AcpError> {
    let Some(policy) = context.terminal.as_deref() else {
        let _ = responder.respond_with_error(AcpError::method_not_found());
        return Ok(());
    };
    let approved = {
        let mut state = context.shared.lock().await;
        let turn = state.turn;
        state.approved.take() == Some(turn)
    };
    if !approved {
        let _ = responder
            .respond_with_error(AcpError::new(ErrorCode::new(-32000), "permission required"));
        return Ok(());
    }
    let executable = match resolve_executable(policy, &request.command) {
        Ok(executable) => executable,
        Err(error) => {
            let _ = responder.respond_with_error(acp_error(error));
            return Ok(());
        }
    };
    let cwd = match request.cwd {
        Some(cwd) if !cwd.as_os_str().is_empty() => {
            match cwd
                .to_str()
                .ok_or_else(Error::invalid)
                .and_then(|cwd| context.workspace.directory(cwd))
            {
                Ok(cwd) => cwd,
                Err(error) => {
                    let _ = responder.respond_with_error(acp_error(error));
                    return Ok(());
                }
            }
        }
        _ => context.workspace.root().to_path_buf(),
    };
    let mut command = Command::new(&executable);
    command
        .args(&request.args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for key in BASE_ENV {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for variable in &request.env {
        if variable.name.is_empty()
            || variable.name.len() > 256
            || variable.name.contains(['=', '\0'])
            || variable.name.chars().any(char::is_control)
            || !environment_allowed(policy, &variable.name)
        {
            let _ = responder.respond_with_error(AcpError::new(
                ErrorCode::INVALID_PARAMS,
                "environment not allowed",
            ));
            return Ok(());
        }
        command.env(&variable.name, &variable.value);
    }
    let limit = request
        .output_byte_limit
        .map(|value| value as usize)
        .unwrap_or(policy.max_output)
        .clamp(1, policy.max_output);
    let mut wrapped = grouped(command);
    let mut child = match wrapped.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = responder.respond_with_error(AcpError::new(
                ErrorCode::new(-32000),
                "failed to start command",
            ));
            return Ok(());
        }
    };
    let output = Arc::new(Mutex::new(TerminalOutput::default()));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout().take() {
        readers.push(tap(stdout, output.clone(), limit));
    }
    if let Some(stderr) = child.stderr().take() {
        readers.push(tap(stderr, output.clone(), limit));
    }
    let (commands, command_rx) = mpsc::channel(1);
    let (exit_tx, exit_rx) = watch::channel(None);
    tokio::spawn(run_terminal(
        child,
        readers,
        exit_tx,
        command_rx,
        policy.max_seconds,
    ));
    let terminal_id = uuid::Uuid::new_v4().to_string();
    context.terminals.lock().await.insert(
        terminal_id.clone(),
        Terminal {
            commands,
            output,
            exit: exit_rx,
        },
    );
    let _ = responder.respond(schema::CreateTerminalResponse::new(terminal_id));
    Ok(())
}

async fn run_terminal(
    mut child: Box<dyn ChildWrapper>,
    readers: Vec<JoinHandle<()>>,
    exit: watch::Sender<Option<Option<i32>>>,
    mut commands: mpsc::Receiver<()>,
    max_seconds: u64,
) {
    let timer = tokio::time::sleep(Duration::from_secs(max_seconds));
    tokio::pin!(timer);
    let mut timed_out = false;
    let mut commands_open = true;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The leader is done; terminate any background members so the
                // output pipes close and the terminal leaves no orphans.
                let _ = child.start_kill();
                break status.code();
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.start_kill();
                break None;
            }
        }
        tokio::select! {
            command = commands.recv(), if commands_open => {
                match command {
                    Some(()) => {
                        let _ = child.start_kill();
                    }
                    None => {
                        commands_open = false;
                        let _ = child.start_kill();
                    }
                }
            }
            _ = &mut timer, if !timed_out => {
                timed_out = true;
                let _ = child.start_kill();
            }
            _ = tokio::time::sleep(TERMINAL_POLL) => {}
        }
    };
    for reader in readers {
        let _ = reader.await;
    }
    let _ = exit.send(Some(code));
}

async fn on_terminal_output(
    context: &ClientContext,
    request: schema::TerminalOutputRequest,
    responder: Responder<schema::TerminalOutputResponse>,
) -> Result<(), AcpError> {
    let id = request.terminal_id.to_string();
    let (output, truncated, exit) = {
        let terminals = context.terminals.lock().await;
        let Some(terminal) = terminals.get(&id) else {
            let _ = responder.respond_with_error(AcpError::invalid_params());
            return Ok(());
        };
        let output = terminal.output.lock().await;
        (
            output.bytes.clone(),
            output.truncated,
            *terminal.exit.borrow(),
        )
    };
    let exit_status = exit
        .flatten()
        .and_then(|code| u32::try_from(code).ok())
        .map(|code| schema::TerminalExitStatus::new().exit_code(code));
    let _ = responder.respond(
        schema::TerminalOutputResponse::new(String::from_utf8_lossy(&output), truncated)
            .exit_status(exit_status),
    );
    Ok(())
}

async fn on_wait_for_exit(
    context: &ClientContext,
    request: schema::WaitForTerminalExitRequest,
    responder: Responder<schema::WaitForTerminalExitResponse>,
    cx: ConnectionTo<Agent>,
) -> Result<(), AcpError> {
    let id = request.terminal_id.to_string();
    let mut exit = {
        let terminals = context.terminals.lock().await;
        let Some(terminal) = terminals.get(&id) else {
            let _ = responder.respond_with_error(AcpError::invalid_params());
            return Ok(());
        };
        terminal.exit.clone()
    };
    let _ = cx.spawn(async move {
        let Ok(code) = exit.wait_for(|value| value.is_some()).await else {
            let _ = responder.respond_with_error(AcpError::internal_error());
            return Ok(());
        };
        let Some(code) = *code else {
            let _ = responder.respond_with_error(AcpError::internal_error());
            return Ok(());
        };
        let _ = responder.respond(schema::WaitForTerminalExitResponse::new(
            schema::TerminalExitStatus::new()
                .exit_code(code.and_then(|code| u32::try_from(code).ok())),
        ));
        Ok(())
    });
    Ok(())
}

async fn on_kill_terminal(
    context: &ClientContext,
    request: schema::KillTerminalRequest,
    responder: Responder<schema::KillTerminalResponse>,
) -> Result<(), AcpError> {
    let id = request.terminal_id.to_string();
    let commands = {
        let terminals = context.terminals.lock().await;
        terminals.get(&id).map(|terminal| terminal.commands.clone())
    };
    if let Some(commands) = commands {
        let _ = commands.try_send(());
    }
    let _ = responder.respond(schema::KillTerminalResponse::new());
    Ok(())
}

async fn on_release_terminal(
    context: &ClientContext,
    request: schema::ReleaseTerminalRequest,
    responder: Responder<schema::ReleaseTerminalResponse>,
) -> Result<(), AcpError> {
    let id = request.terminal_id.to_string();
    let terminal = context.terminals.lock().await.remove(&id);
    if let Some(terminal) = terminal {
        let _ = terminal.commands.try_send(());
    }
    let _ = responder.respond(schema::ReleaseTerminalResponse::new());
    Ok(())
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, captured: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0_u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut captured = captured.lock().await;
                captured.extend_from_slice(&buffer[..read]);
                if captured.len() > STDERR_BYTES {
                    let excess = captured.len() - STDERR_BYTES;
                    captured.drain(..excess);
                }
            }
        }
    }
}

const BASE_ENV: [&str; 7] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "LANG",
    "LC_ALL",
    "TERM",
    "SYSTEMROOT",
];

fn command_allowed(policy: &TerminalLimits, command: &str) -> bool {
    // Exact match only: a path-named binary must be listed explicitly, so a
    // bare allowlist entry cannot be redirected to an arbitrary path.
    policy.allowed.iter().any(|allowed| allowed == command)
}

/// Resolves a bare allowed command with the host's PATH, never the child's
/// environment. An absolute allowlist entry is used as-is.
fn resolve_executable(policy: &TerminalLimits, command: &str) -> Result<PathBuf, Error> {
    if !command_allowed(policy, command) {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let path = Path::new(command);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path.components().count() != 1 {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    which::which(command).map_err(|_| Error::new(ErrorKind::Unavailable))
}

/// Only names the binding lists may be set by the terminal request. A
/// denylist cannot enumerate every interpreter hook (`PYTHONSTARTUP`,
/// `NODE_OPTIONS`, `BASH_ENV`, ...), so the unlisted default is refusal.
fn environment_allowed(policy: &TerminalLimits, name: &str) -> bool {
    policy.allowed_env.iter().any(|allowed| allowed == name)
}

fn tap(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    output: Arc<Mutex<TerminalOutput>>,
    limit: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut output = output.lock().await;
                    let room = limit.saturating_sub(output.bytes.len());
                    if room > 0 {
                        output.bytes.extend_from_slice(&buffer[..read.min(room)]);
                    }
                    if read > room {
                        output.truncated = true;
                    }
                }
            }
        }
    })
}

fn relative_to(workspace: &Workspace, path: &Path) -> Result<String, Error> {
    let relative = path.strip_prefix(workspace.root()).map_err(|_| Error {
        kind: ErrorKind::PermissionRequired,
        reason: Some("outside_workspace"),
    })?;
    relative
        .to_str()
        .filter(|relative| !relative.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))
}

fn acp_error(error: Error) -> AcpError {
    let code = match error.kind {
        ErrorKind::PermissionRequired => ErrorCode::new(-32000),
        ErrorKind::NotFound | ErrorKind::InvalidRequest => ErrorCode::INVALID_PARAMS,
        _ => ErrorCode::INTERNAL_ERROR,
    };
    AcpError::new(code, "request failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_plugin::text_block;

    #[cfg(unix)]
    fn agent_script() -> String {
        r#"
import json, sys
session = "s1"
prompt_id = None
permission_done = False

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = msg["id"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}, "messageId": "m1"}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "fs/read_text_file", "params": {"sessionId": session, "path": sys.argv[1] + "/note.txt"}})
    elif method == "session/cancel":
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "cancelled"}})
    elif msg.get("id") == 900:
        # The fake reads the file through the client and then asks permission.
        send({"jsonrpc": "2.0", "id": 901, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "read"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif msg.get("id") == 901:
        selection = msg["result"]["outcome"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(selection)}, "messageId": "m2"}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    /// One turn a test drives.
    enum Drive<'a> {
        Prompt(&'a [ContentBlock]),
        Respond(&'a str, &'a RequestPermissionOutcome),
    }

    /// Runs one turn and returns the updates it streamed with its end. The
    /// turn closes the update stream when it returns, so collecting cannot
    /// wait for a turn that never ends.
    async fn run_turn(session: &mut AcpSession, drive: Drive<'_>) -> (Vec<SessionUpdate>, Turn) {
        let (updates, mut received) = mpsc::channel(MAX_UPDATES);
        let collect = async {
            let mut seen = Vec::new();
            while let Some(update) = received.recv().await {
                seen.push(update);
            }
            seen
        };
        let turn = async {
            match drive {
                Drive::Prompt(prompt) => session.prompt(prompt, updates).await,
                Drive::Respond(request_id, outcome) => {
                    session.respond(request_id, outcome, updates).await
                }
            }
        };
        let (seen, turn) = tokio::join!(collect, turn);
        (seen, turn.unwrap())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_updates_permission_and_stop_are_mapped() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "file-content").unwrap();
        let mut session = start_agent(temp.path(), &agent_script(), None).await;
        let (updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        assert!(matches!(turn.state, TurnState::Waiting(_)));
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let request_id = waiting.request_id;
        let options = waiting.options;
        assert_eq!(options[0].option_id.to_string(), "allow");
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "hello")
        )));
        let (updates, turn) = run_turn(
            &mut session,
            Drive::Respond(
                &request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        let TurnState::Stopped(stop) = turn.state else {
            panic!("expected stop");
        };
        assert_eq!(stop, StopReason::EndTurn);
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("allow"))
        )));
        session.kill().await;
    }

    #[cfg(unix)]
    fn unknown_stop_agent_script() -> String {
        r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": "s1"}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "bogus"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unknown_stop_reason_fails_instead_of_ending_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(temp.path(), &unknown_stop_agent_script(), None).await;
        let (updates, _received) = mpsc::channel(MAX_UPDATES);
        let error = session
            .prompt(&[text_block("go")], updates)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ExternalFailure);
        session.kill().await;
    }

    #[test]
    fn acp_error_mapping_preserves_host_error_kinds() {
        assert_eq!(
            acp_error(Error::new(ErrorKind::PermissionRequired)).code,
            ErrorCode::new(-32000)
        );
        assert_eq!(acp_error(Error::invalid()).code, ErrorCode::INVALID_PARAMS);
        assert_eq!(
            acp_error(Error::unavailable()).code,
            ErrorCode::INTERNAL_ERROR
        );
    }

    #[cfg(unix)]
    fn terminal_kill_agent_script() -> String {
        r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

session = "s1"
prompt_id = None
terminal_id = None
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "started"}}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "sleep", "args": ["30"]}})
    elif mid == 901:
        terminal_id = msg["result"]["terminalId"]
        send({"jsonrpc": "2.0", "id": 902, "method": "terminal/wait_for_exit", "params": {"sessionId": session, "terminalId": terminal_id}})
        send({"jsonrpc": "2.0", "id": 903, "method": "terminal/kill", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 903:
        send({"jsonrpc": "2.0", "id": 904, "method": "terminal/output", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 904:
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_kill_is_processed_while_wait_for_exit_is_pending() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(
            temp.path(),
            &terminal_kill_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["sleep".into()],
                allowed_env: vec![],
                max_seconds: 10,
                max_output: 64,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let allowed =
            RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new("allow"));
        let (_updates, turn) = tokio::time::timeout(
            Duration::from_secs(5),
            run_turn(&mut session, Drive::Respond(&waiting.request_id, &allowed)),
        )
        .await
        .expect("terminal/kill must not wait for the pending wait_for_exit");
        assert!(matches!(
            turn.state,
            TurnState::Stopped(StopReason::EndTurn)
        ));
        session.kill().await;
    }

    #[cfg(unix)]
    fn process_group_agent_script() -> String {
        r#"
import json, os, sys, time

session = "s1"
prompt_id = None
pid_file = os.path.join(sys.argv[1], "grandchild.pid")

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "sh", "args": ["-c", "sleep 30 & echo $! > " + pid_file + "; wait"]}})
    elif mid == 901:
        terminal_id = msg["result"]["terminalId"]
        for _ in range(200):
            if os.path.exists(pid_file):
                break
            time.sleep(0.01)
        send({"jsonrpc": "2.0", "id": 902, "method": "terminal/kill", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 902:
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_kill_terminates_the_whole_process_group() {
        use rustix::process::{Pid, test_kill_process};
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild.pid");
        let mut session = start_agent(
            temp.path(),
            &process_group_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["sh".into()],
                allowed_env: vec![],
                max_seconds: 10,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (_updates, turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        assert!(matches!(
            turn.state,
            TurnState::Stopped(StopReason::EndTurn)
        ));

        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the terminal child did not record its background pid");

        let gone = tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                if test_kill_process(Pid::from_raw(pid).unwrap()).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "grandchild {pid} survived terminal/kill");
        session.kill().await;
    }

    #[cfg(unix)]
    async fn start_agent(
        dir: &std::path::Path,
        script: &str,
        terminal: Option<TerminalLimits>,
    ) -> AcpSession {
        let script_path = dir.join("agent.py");
        std::fs::write(&script_path, script).unwrap();
        AcpSession::start(AcpConfig {
            command: vec![
                "python3".into(),
                script_path.to_str().unwrap().into(),
                dir.to_str().unwrap().into(),
            ],
            workspace: Workspace::open(dir.to_str().unwrap()).unwrap(),
            cwd: std::fs::canonicalize(dir).unwrap(),
            terminal,
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    fn unused_approval_agent_script() -> String {
        r#"
import json, sys

session = "s1"
prompt_id = None
turn = 0
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        turn += 1
        if turn == 1:
            send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "allow"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
        else:
            send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "touch", "args": [sys.argv[1] + "/created"]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
    elif mid == 901:
        text = "denied" if "error" in msg else "ran"
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unused_terminal_approval_does_not_survive_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(
            temp.path(),
            &unused_approval_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["touch".into()],
                allowed_env: vec![],
                max_seconds: 5,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("first")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (_updates, turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        assert!(matches!(
            turn.state,
            TurnState::Stopped(StopReason::EndTurn)
        ));

        let (updates, _turn) = run_turn(&mut session, Drive::Prompt(&[text_block("second")])).await;
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "denied")
        )));
        assert!(!temp.path().join("created").exists());
        session.kill().await;
    }

    #[cfg(unix)]
    fn path_override_agent_script() -> String {
        r#"
import json, sys

session = "s1"
prompt_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "sh", "args": ["-c", "touch " + sys.argv[1] + "/pwned"], "env": [{"name": "PATH", "value": sys.argv[1] + "/evil"}]}})
    elif mid == 901:
        text = "rejected" if "error" in msg else "ran"
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_terminal_cannot_override_path_to_redirect_the_allowlisted_command() {
        let temp = tempfile::tempdir().unwrap();
        let evil = temp.path().join("evil");
        std::fs::create_dir(&evil).unwrap();
        let mut session = start_agent(
            temp.path(),
            &path_override_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["sh".into()],
                allowed_env: vec![],
                max_seconds: 5,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (updates, _turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "rejected")
        )));
        assert!(!temp.path().join("pwned").exists());
        session.kill().await;
    }

    #[cfg(unix)]
    fn allowed_env_agent_script() -> String {
        r#"
import json, sys

session = "s1"
prompt_id = None
terminal_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "sh", "args": ["-c", "echo $MARKER"], "env": [{"name": "MARKER", "value": "passed"}]}})
    elif mid == 901:
        terminal_id = msg["result"]["terminalId"]
        send({"jsonrpc": "2.0", "id": 902, "method": "terminal/wait_for_exit", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 902:
        send({"jsonrpc": "2.0", "id": 903, "method": "terminal/output", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 903:
        text = "passed" if "passed" in msg["result"]["output"] else "missing"
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_allowed_environment_name_reaches_the_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(
            temp.path(),
            &allowed_env_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["sh".into()],
                allowed_env: vec!["MARKER".into()],
                max_seconds: 5,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (updates, _turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "passed")
        )));
        session.kill().await;
    }

    #[cfg(unix)]
    fn flood_agent_script() -> String {
        r#"
import json, sys

session = "s1"
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        for index in range(4097):
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn too_many_updates_fail_the_turn_even_when_the_consumer_keeps_up() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(temp.path(), &flood_agent_script(), None).await;
        // Drain as fast as the turn produces: the per-turn count, not the
        // intermediate channel, must end the turn.
        let (updates, mut received) = mpsc::channel(MAX_UPDATES);
        let drain = tokio::spawn(async move { while received.recv().await.is_some() {} });
        let error = session
            .prompt(&[text_block("go")], updates)
            .await
            .unwrap_err();
        drain.await.unwrap();
        assert_eq!(error.kind, ErrorKind::ExternalFailure);
        assert_eq!(session.shared.lock().await.failed, Some("acp_update_limit"));
        session.kill().await;
    }

    #[cfg(unix)]
    fn waiting_flood_agent_script() -> String {
        r#"
import json, sys

session = "s1"
prompt_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
        for index in range(4097):
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_flood_while_waiting_for_permission_ends_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(temp.path(), &waiting_flood_agent_script(), None).await;
        let (updates, mut received) = mpsc::channel(MAX_UPDATES);
        let collect = tokio::spawn(async move { while received.recv().await.is_some() {} });
        let turn = session.prompt(&[text_block("go")], updates).await.unwrap();
        collect.await.unwrap();
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let allowed =
            RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new("allow"));
        let (updates, mut received) = mpsc::channel(MAX_UPDATES);
        let collect = tokio::spawn(async move { while received.recv().await.is_some() {} });
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            session.respond(&waiting.request_id, &allowed, updates),
        )
        .await
        .expect("the backlog bound must end a waiting turn")
        .unwrap_err();
        collect.await.unwrap();
        assert_eq!(error.kind, ErrorKind::ExternalFailure);
        assert!(session.shared.lock().await.failed.is_some());
        session.kill().await;
    }

    #[cfg(unix)]
    fn natural_exit_agent_script() -> String {
        r#"
import json, sys, time

session = "s1"
prompt_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "sh", "args": ["-c", "exit 7"]}})
    elif mid == 901:
        time.sleep(0.3)
        send({"jsonrpc": "2.0", "id": 902, "method": "terminal/output", "params": {"sessionId": session, "terminalId": msg["result"]["terminalId"]}})
    elif mid == 902:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(msg.get("result", {}).get("exitStatus"))}}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#
        .into()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_output_reports_a_natural_exit_without_wait_for_exit() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = start_agent(
            temp.path(),
            &natural_exit_agent_script(),
            Some(TerminalLimits {
                allowed: vec!["sh".into()],
                allowed_env: vec![],
                max_seconds: 5,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (updates, _turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        let report = updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<String>();
        assert!(report.contains("exitCode"), "missing exit status: {report}");
        assert!(report.contains('7'), "wrong exit status: {report}");
        session.kill().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_release_terminates_the_whole_process_group() {
        use rustix::process::{Pid, test_kill_process};
        let temp = tempfile::tempdir().unwrap();
        let script = process_group_agent_script().replace("terminal/kill", "terminal/release");
        let pid_file = temp.path().join("grandchild.pid");
        let mut session = start_agent(
            temp.path(),
            &script,
            Some(TerminalLimits {
                allowed: vec!["sh".into()],
                allowed_env: vec![],
                max_seconds: 10,
                max_output: 4096,
            }),
        )
        .await;
        let (_updates, turn) = run_turn(&mut session, Drive::Prompt(&[text_block("go")])).await;
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let (_updates, turn) = run_turn(
            &mut session,
            Drive::Respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await;
        assert!(matches!(
            turn.state,
            TurnState::Stopped(StopReason::EndTurn)
        ));

        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the terminal child did not record its background pid");
        let gone = tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                if test_kill_process(Pid::from_raw(pid).unwrap()).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "grandchild {pid} survived terminal/release");
        session.kill().await;
    }
}
