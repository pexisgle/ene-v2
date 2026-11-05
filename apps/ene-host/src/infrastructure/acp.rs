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
    Agent, ByteStreams, Client, Error as AcpError, ErrorCode,
    schema::ProtocolVersion,
    v1::{ConnectionTo, Responder, schema},
};
use ene_plugin::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome, SessionUpdate,
    StopReason, ToolCallUpdate,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_UPDATES: usize = 4096;
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
    pub(crate) max_seconds: u64,
    pub(crate) max_output: usize,
}

struct Terminal {
    child: Arc<Mutex<Child>>,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    exit: Arc<Mutex<Option<Option<i32>>>>,
    readers: Arc<Mutex<Vec<JoinHandle<()>>>>,
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

#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub(crate) updates: Vec<SessionUpdate>,
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

struct State {
    session: Option<schema::SessionId>,
    updates: Vec<SessionUpdate>,
    turn: Option<oneshot::Sender<Result<Turn, String>>>,
    permission: Option<Permission>,
    approved: bool,
    /// Increments for every prompt so a stale completion cannot resolve a
    /// later turn.
    sequence: u64,
    active: bool,
    failed: Option<&'static str>,
}

impl State {
    fn new() -> Self {
        Self {
            session: None,
            updates: Vec::new(),
            turn: None,
            permission: None,
            approved: false,
            sequence: 0,
            active: false,
            failed: None,
        }
    }
}

pub(crate) struct AcpSession {
    child: Child,
    connection: ConnectionTo<Agent>,
    driver: JoinHandle<()>,
    prompt_task: Option<JoinHandle<()>>,
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
            .env_clear()
            .kill_on_drop(true);
        for key in BASE_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command.spawn().map_err(|error| Error {
            kind: ErrorKind::ExternalFailure,
            reason: Some(if error.kind() == std::io::ErrorKind::NotFound {
                "acp_command_not_found"
            } else {
                "acp_spawn_failed"
            }),
        })?;
        let stdin = child.stdin.take().ok_or_else(Error::unavailable)?;
        let stdout = child.stdout.take().ok_or_else(Error::unavailable)?;
        let stderr = child.stderr.take().ok_or_else(Error::unavailable)?;

        let shared = Arc::new(Mutex::new(State::new()));
        let terminals = Arc::new(Mutex::new(HashMap::<String, Terminal>::new()));
        let terminal = config.terminal.map(Arc::new);
        let terminal_enabled = terminal.is_some();
        let context = ClientContext {
            shared: shared.clone(),
            workspace: config.workspace,
            terminal,
            terminals,
        };
        let captured = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, captured.clone()));
        let shutdown = CancellationToken::new();
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let (ready_tx, ready_rx) =
            oneshot::channel::<Result<(ConnectionTo<Agent>, schema::SessionId), AcpError>>();
        let cwd = config.cwd;

        let driver_shared = shared.clone();
        let driver_stderr = captured.clone();
        let driver_shutdown = shutdown.clone();
        let session_shutdown = driver_shutdown.clone();
        let driver_context = context.clone();
        let driver = tokio::spawn(async move {
            let result = Client
                .v1()
                .name("ene")
                .on_receive_notification(
                    {
                        let context = driver_context.clone();
                        async move |notification: schema::SessionNotification,
                                    _cx: ConnectionTo<Agent>| {
                            on_update(&context, notification).await
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::RequestPermissionRequest,
                                    responder: Responder<schema::RequestPermissionResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_permission(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::ReadTextFileRequest,
                                    responder: Responder<schema::ReadTextFileResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_read_file(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::WriteTextFileRequest,
                                    responder: Responder<schema::WriteTextFileResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_write_file(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::CreateTerminalRequest,
                                    responder: Responder<schema::CreateTerminalResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_create_terminal(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::TerminalOutputRequest,
                                    responder: Responder<schema::TerminalOutputResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_terminal_output(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::WaitForTerminalExitRequest,
                                    responder: Responder<schema::WaitForTerminalExitResponse>,
                                    cx: ConnectionTo<Agent>| {
                            on_wait_for_exit(&context, request, responder, cx).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::KillTerminalRequest,
                                    responder: Responder<schema::KillTerminalResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_kill_terminal(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        let context = driver_context.clone();
                        async move |request: schema::ReleaseTerminalRequest,
                                    responder: Responder<schema::ReleaseTerminalResponse>,
                                    _cx: ConnectionTo<Agent>| {
                            on_release_terminal(&context, request, responder).await
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let initialize = timeout(
                        REQUEST_TIMEOUT,
                        cx.send_request(initialize_request(terminal_enabled))
                            .block_task(),
                    )
                    .await
                    .map_err(|_| AcpError::request_cancelled())??;
                    let _ = initialize;
                    let created = timeout(
                        REQUEST_TIMEOUT,
                        cx.send_request(schema::NewSessionRequest::new(cwd))
                            .block_task(),
                    )
                    .await
                    .map_err(|_| AcpError::request_cancelled())??;
                    if session_shutdown.is_cancelled() {
                        return Ok(());
                    }
                    let _ = ready_tx.send(Ok((cx.clone(), created.session_id)));
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
            Ok(Ok(Ok((connection, session_id)))) => {
                shared.lock().await.session = Some(session_id);
                Ok(Self {
                    child,
                    connection,
                    driver,
                    prompt_task: None,
                    shared,
                    stderr: captured,
                    stderr_task,
                    shutdown,
                })
            }
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

    pub(crate) async fn prompt(&mut self, prompt: &[ContentBlock]) -> Result<Turn, Error> {
        let (rx, sequence, session) = {
            let mut state = self.shared.lock().await;
            if state.failed.is_some() {
                return Err(Error::new(ErrorKind::ExternalFailure));
            }
            if state.turn.is_some() || state.permission.is_some() {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            state.sequence += 1;
            state.active = true;
            state.updates.clear();
            let (tx, rx) = oneshot::channel();
            state.turn = Some(tx);
            (
                rx,
                state.sequence,
                state
                    .session
                    .clone()
                    .ok_or_else(|| Error::new(ErrorKind::StateConflict))?,
            )
        };
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        let request = schema::PromptRequest::new(session, prompt.to_vec());
        let connection = self.connection.clone();
        let shared = self.shared.clone();
        self.prompt_task = Some(tokio::spawn(async move {
            let result = connection.send_request(request).block_task().await;
            finish(&shared, sequence, result).await;
        }));
        self.await_turn(rx).await
    }

    pub(crate) async fn respond(
        &mut self,
        request_id: &str,
        outcome: &RequestPermissionOutcome,
    ) -> Result<Turn, Error> {
        let (permission, rx) = {
            let mut state = self.shared.lock().await;
            let Some(permission) = state.permission.take() else {
                return Err(Error::new(ErrorKind::StateConflict));
            };
            if permission.request_id != request_id {
                state.permission = Some(permission);
                return Err(Error::new(ErrorKind::StateConflict));
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
                state.approved = true;
            }
            let (tx, rx) = oneshot::channel();
            state.turn = Some(tx);
            (permission, rx)
        };
        if permission
            .responder
            .respond(schema::RequestPermissionResponse::new(outcome.clone()))
            .is_err()
        {
            self.shared.lock().await.turn = None;
            return Err(Error::unavailable());
        }
        self.await_turn(rx).await
    }

    pub(crate) async fn cancel(&mut self) -> Result<(), Error> {
        let session = self.shared.lock().await.session.clone();
        let Some(session) = session else {
            return Ok(());
        };
        self.connection
            .send_notification(schema::CancelNotification::new(session))
            .map_err(|_| Error::unavailable())
    }

    pub(crate) async fn kill(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        self.driver.abort();
        self.stderr_task.abort();
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
    }

    async fn await_turn(
        &mut self,
        rx: oneshot::Receiver<Result<Turn, String>>,
    ) -> Result<Turn, Error> {
        match timeout(TURN_TIMEOUT, rx).await {
            Ok(Ok(Ok(turn))) => Ok(turn),
            Ok(Ok(Err(reason))) => {
                let captured = self.stderr.lock().await;
                eprintln!(
                    "ene-host acp: {reason}: {}",
                    String::from_utf8_lossy(&captured)
                );
                drop(captured);
                Err(Error {
                    kind: ErrorKind::ExternalFailure,
                    reason: Some("acp_agent_failed"),
                })
            }
            Ok(Err(_)) => {
                reset_turn(&self.shared).await;
                Err(Error::new(ErrorKind::Unavailable))
            }
            Err(_) => {
                let _ = self.cancel().await;
                if let Some(task) = self.prompt_task.take() {
                    task.abort();
                }
                reset_turn(&self.shared).await;
                Err(Error::new(ErrorKind::Unavailable))
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

async fn finish(
    shared: &Arc<Mutex<State>>,
    sequence: u64,
    result: Result<schema::PromptResponse, AcpError>,
) {
    let mut state = shared.lock().await;
    if state.sequence != sequence {
        return;
    }
    state.active = false;
    let updates = std::mem::take(&mut state.updates);
    if let Some(permission) = state.permission.take() {
        let _ = permission
            .responder
            .respond(schema::RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
    }
    let Some(turn) = state.turn.take() else {
        return;
    };
    match result {
        Ok(response) => {
            let _ = turn.send(Ok(Turn {
                updates,
                state: TurnState::Stopped(response.stop_reason),
            }));
        }
        Err(error) => {
            let _ = turn.send(Err(format!("acp_agent_failed: {error}")));
        }
    }
}

async fn reset_turn(shared: &Arc<Mutex<State>>) {
    let mut state = shared.lock().await;
    state.active = false;
    state.sequence += 1;
    state.updates.clear();
    state.turn = None;
    if let Some(permission) = state.permission.take() {
        let _ = permission
            .responder
            .respond(schema::RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
    }
}

async fn fail(shared: &Arc<Mutex<State>>, stderr: &Arc<Mutex<Vec<u8>>>, reason: String) {
    let mut state = shared.lock().await;
    state.active = false;
    state.failed = Some("acp_agent_failed");
    state.sequence += 1;
    state.updates.clear();
    if let Some(turn) = state.turn.take() {
        let _ = turn.send(Err(reason.clone()));
    }
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

async fn on_update(
    context: &ClientContext,
    notification: schema::SessionNotification,
) -> Result<(), AcpError> {
    let mut state = context.shared.lock().await;
    if !state.active {
        return Ok(());
    }
    if state.session.as_ref() != Some(&notification.session_id) {
        return Err(AcpError::invalid_request());
    }
    if let Some(update) = map_update(notification.update) {
        if state.updates.len() >= MAX_UPDATES {
            return Err(AcpError::internal_error().data("update_limit"));
        }
        state.updates.push(update);
    }
    Ok(())
}

async fn on_permission(
    context: &ClientContext,
    request: schema::RequestPermissionRequest,
    responder: Responder<schema::RequestPermissionResponse>,
) -> Result<(), AcpError> {
    let mut state = context.shared.lock().await;
    if !state.active
        || state.session.as_ref() != Some(&request.session_id)
        || state.permission.is_some()
        || state.turn.is_none()
    {
        let _ = responder.respond(schema::RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
        return Ok(());
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    state.permission = Some(Permission {
        request_id: request_id.clone(),
        options: request.options.clone(),
        responder,
    });
    let turn = state
        .turn
        .take()
        .expect("a permission request requires an active turn");
    let updates = std::mem::take(&mut state.updates);
    let _ = turn.send(Ok(Turn {
        updates,
        state: TurnState::Waiting(Box::new(Waiting {
            request_id,
            tool_call: request.tool_call,
            options: request.options,
        })),
    }));
    Ok(())
}

async fn on_read_file(
    context: &ClientContext,
    request: schema::ReadTextFileRequest,
    responder: Responder<schema::ReadTextFileResponse>,
) -> Result<(), AcpError> {
    match relative_to(&context.workspace, &request.path)
        .and_then(|path| context.workspace.read(&path, request.line, request.limit))
    {
        Ok(read) => {
            let _ = responder.respond(schema::ReadTextFileResponse::new(read.content));
        }
        Err(error) => {
            let _ = responder.respond_with_error(acp_error(error));
        }
    }
    Ok(())
}

async fn on_write_file(
    context: &ClientContext,
    request: schema::WriteTextFileRequest,
    responder: Responder<schema::WriteTextFileResponse>,
) -> Result<(), AcpError> {
    match relative_to(&context.workspace, &request.path)
        .and_then(|path| context.workspace.write(&path, &request.content))
    {
        Ok(()) => {
            let _ = responder.respond(schema::WriteTextFileResponse::new());
        }
        Err(error) => {
            let _ = responder.respond_with_error(acp_error(error));
        }
    }
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
        let approved = state.approved;
        state.approved = false;
        approved
    };
    if !approved {
        let _ = responder
            .respond_with_error(AcpError::new(ErrorCode::new(-32000), "permission required"));
        return Ok(());
    }
    if !command_allowed(policy, &request.command) {
        let _ = responder
            .respond_with_error(AcpError::new(ErrorCode::new(-32000), "command not allowed"));
        return Ok(());
    }
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
    let mut command = Command::new(&request.command);
    command
        .args(&request.args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .kill_on_drop(true);
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
        {
            let _ = responder.respond_with_error(AcpError::invalid_params());
            return Ok(());
        }
        command.env(&variable.name, &variable.value);
    }
    let limit = request
        .output_byte_limit
        .map(|value| value as usize)
        .unwrap_or(policy.max_output)
        .clamp(1, policy.max_output);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = responder.respond_with_error(AcpError::new(
                ErrorCode::new(-32000),
                "failed to start command",
            ));
            return Ok(());
        }
    };
    let output = Arc::new(Mutex::new(Vec::new()));
    let truncated = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(tap(stdout, output.clone(), truncated.clone(), limit));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(tap(stderr, output.clone(), truncated.clone(), limit));
    }
    let child = Arc::new(Mutex::new(child));
    let exit = Arc::new(Mutex::new(None));
    let terminal_id = uuid::Uuid::new_v4().to_string();
    context.terminals.lock().await.insert(
        terminal_id.clone(),
        Terminal {
            child: child.clone(),
            output,
            truncated,
            exit,
            readers: Arc::new(Mutex::new(readers)),
        },
    );
    let seconds = policy.max_seconds;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let _ = child.lock().await.start_kill();
    });
    let _ = responder.respond(schema::CreateTerminalResponse::new(terminal_id));
    Ok(())
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
        (
            terminal.output.lock().await.clone(),
            terminal.truncated.load(Ordering::Relaxed),
            *terminal.exit.lock().await,
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
    let (child, exit, readers) = {
        let terminals = context.terminals.lock().await;
        let Some(terminal) = terminals.get(&id) else {
            let _ = responder.respond_with_error(AcpError::invalid_params());
            return Ok(());
        };
        (
            terminal.child.clone(),
            terminal.exit.clone(),
            terminal.readers.clone(),
        )
    };
    let _ = cx.spawn(async move {
        let status = loop {
            match child.lock().await.try_wait() {
                Ok(Some(status)) => break Some(status.code()),
                Ok(None) => tokio::time::sleep(TERMINAL_POLL).await,
                Err(_) => break None,
            }
        };
        let Some(code) = status else {
            let _ = responder.respond_with_error(AcpError::internal_error());
            return Ok(());
        };
        *exit.lock().await = Some(code);
        let readers = std::mem::take(&mut *readers.lock().await);
        for reader in readers {
            let _ = reader.await;
        }
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
    let child = {
        let terminals = context.terminals.lock().await;
        terminals.get(&id).map(|terminal| terminal.child.clone())
    };
    if let Some(child) = child {
        let _ = child.lock().await.start_kill();
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
        let _ = terminal.child.lock().await.start_kill();
    }
    let _ = responder.respond(schema::ReleaseTerminalResponse::new());
    Ok(())
}

fn map_update(update: schema::SessionUpdate) -> Option<SessionUpdate> {
    Some(match update {
        schema::SessionUpdate::AgentMessageChunk(chunk) => SessionUpdate::AgentMessageChunk(chunk),
        schema::SessionUpdate::AgentThoughtChunk(chunk) => SessionUpdate::AgentThoughtChunk(chunk),
        schema::SessionUpdate::ToolCall(call) => SessionUpdate::ToolCall(call),
        schema::SessionUpdate::ToolCallUpdate(update) => SessionUpdate::ToolCallUpdate(update),
        schema::SessionUpdate::Plan(plan) => SessionUpdate::Plan(plan),
        _ => return None,
    })
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

fn tap(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    limit: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut output = output.lock().await;
                    let room = limit.saturating_sub(output.len());
                    if room > 0 {
                        output.extend_from_slice(&buffer[..read.min(room)]);
                    }
                    if read > room {
                        truncated.store(true, Ordering::Relaxed);
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
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use ene_plugin::{ContentChunk, text_block};

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

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_updates_permission_and_stop_are_mapped() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "file-content").unwrap();
        let script_path = temp.path().join("agent.py");
        std::fs::write(&script_path, agent_script()).unwrap();
        let config = AcpConfig {
            command: vec![
                "python3".into(),
                script_path.to_str().unwrap().into(),
                temp.path().to_str().unwrap().into(),
            ],
            workspace: Workspace::open(temp.path().to_str().unwrap()).unwrap(),
            cwd: std::fs::canonicalize(temp.path()).unwrap(),
            terminal: None,
        };
        let mut session = AcpSession::start(config).await.unwrap();
        let turn = session.prompt(&[text_block("go")]).await.unwrap();
        assert!(matches!(turn.state, TurnState::Waiting(_)));
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let request_id = waiting.request_id;
        let options = waiting.options;
        assert_eq!(options[0].option_id.to_string(), "allow");
        assert!(turn.updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "hello")
        )));
        let turn = session
            .respond(
                &request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            )
            .await
            .unwrap();
        let TurnState::Stopped(stop) = turn.state else {
            panic!("expected stop");
        };
        assert_eq!(stop, StopReason::EndTurn);
        assert!(turn.updates.iter().any(|update| matches!(
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
        let script_path = temp.path().join("agent.py");
        std::fs::write(&script_path, unknown_stop_agent_script()).unwrap();
        let config = AcpConfig {
            command: vec![
                "python3".into(),
                script_path.to_str().unwrap().into(),
                temp.path().to_str().unwrap().into(),
            ],
            workspace: Workspace::open(temp.path().to_str().unwrap()).unwrap(),
            cwd: std::fs::canonicalize(temp.path()).unwrap(),
            terminal: None,
        };
        let mut session = AcpSession::start(config).await.unwrap();
        let error = session.prompt(&[text_block("go")]).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::ExternalFailure);
        session.kill().await;
    }

    #[tokio::test]
    async fn late_updates_are_dropped_and_the_update_limit_is_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let shared = Arc::new(Mutex::new(State::new()));
        {
            let mut state = shared.lock().await;
            state.session = Some(schema::SessionId::new("s"));
            state.active = true;
        }
        let context = ClientContext {
            shared: shared.clone(),
            workspace: Workspace::open(temp.path().to_str().unwrap()).unwrap(),
            terminal: None,
            terminals: Arc::new(Mutex::new(HashMap::new())),
        };
        let notification = || {
            schema::SessionNotification::new(
                "s",
                schema::SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block("hi"))),
            )
        };
        on_update(&context, notification()).await.unwrap();
        assert_eq!(shared.lock().await.updates.len(), 1);

        let (tx, rx) = oneshot::channel();
        {
            let mut state = shared.lock().await;
            state.turn = Some(tx);
            state.sequence = 1;
        }
        finish(
            &shared,
            1,
            Ok(schema::PromptResponse::new(StopReason::EndTurn)),
        )
        .await;
        let turn = rx.await.unwrap().unwrap();
        assert_eq!(turn.updates.len(), 1);
        on_update(&context, notification()).await.unwrap();
        assert!(shared.lock().await.updates.is_empty());

        {
            let mut state = shared.lock().await;
            state.active = true;
            state.updates = vec![SessionUpdate::Plan(schema::Plan::new(vec![])); MAX_UPDATES];
        }
        let error = on_update(&context, notification()).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
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
        let script_path = temp.path().join("agent.py");
        std::fs::write(&script_path, terminal_kill_agent_script()).unwrap();
        let config = AcpConfig {
            command: vec![
                "python3".into(),
                script_path.to_str().unwrap().into(),
                temp.path().to_str().unwrap().into(),
            ],
            workspace: Workspace::open(temp.path().to_str().unwrap()).unwrap(),
            cwd: std::fs::canonicalize(temp.path()).unwrap(),
            terminal: Some(TerminalLimits {
                allowed: vec!["sleep".into()],
                max_seconds: 10,
                max_output: 64,
            }),
        };
        let mut session = AcpSession::start(config).await.unwrap();
        let turn = session.prompt(&[text_block("go")]).await.unwrap();
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let turn = tokio::time::timeout(
            Duration::from_secs(5),
            session.respond(
                &waiting.request_id,
                &RequestPermissionOutcome::Selected(ene_plugin::SelectedPermissionOutcome::new(
                    "allow",
                )),
            ),
        )
        .await
        .expect("terminal/kill must not wait for the pending wait_for_exit")
        .unwrap();
        assert!(matches!(
            turn.state,
            TurnState::Stopped(StopReason::EndTurn)
        ));
        session.kill().await;
    }
}
