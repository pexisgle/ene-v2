//! Minimal ACP v1 client over an explicit stdio subprocess.
//!
//! The process is configured by the host (never a shell), receives only a
//! filtered environment, and advertises `fs` read/write with `terminal` off.
//! Session updates, permission requests and stop reasons are mapped to the
//! host's ACP-shaped harness vocabulary.
use crate::{
    domain::{Error, ErrorKind},
    infrastructure::workspace::Workspace,
};
use ene_plugin::{
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionKind, Plan,
    RequestPermissionOutcome, SessionUpdate, StopReason, ToolCall, ToolCallUpdate,
};
use serde_json::{Value, json};
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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_UPDATES: usize = 4096;
const STDERR_BYTES: usize = 4096;

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
    rpc_id: u64,
    options: Vec<PermissionOption>,
}

#[derive(Clone)]
struct ClientContext {
    shared: Arc<Mutex<State>>,
    writer: mpsc::Sender<Value>,
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
    session_id: String,
    updates: Vec<SessionUpdate>,
    turn: Option<oneshot::Sender<std::result::Result<Turn, String>>>,
    prompt_id: Option<u64>,
    permission: Option<Permission>,
    approved: bool,
    pending: HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>,
    next_id: u64,
}

impl State {
    fn new() -> Self {
        Self {
            session_id: String::new(),
            updates: Vec::new(),
            turn: None,
            prompt_id: None,
            permission: None,
            approved: false,
            pending: HashMap::new(),
            next_id: 1,
        }
    }
    fn request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

pub(crate) struct AcpSession {
    child: Child,
    writer: mpsc::Sender<Value>,
    shared: Arc<Mutex<State>>,
    tasks: Vec<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
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
        let terminal_enabled = config.terminal.is_some();
        let terminal = config.terminal.map(Arc::new);
        let (writer_tx, mut writer_rx) = mpsc::channel::<Value>(64);
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(value) = writer_rx.recv().await {
                let Ok(mut line) = serde_json::to_vec(&value) else {
                    break;
                };
                line.push(b'\n');
                if stdin.write_all(&line).await.is_err() {
                    break;
                }
            }
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, captured.clone()));
        let reader = tokio::spawn(read_loop(
            stdout,
            shared.clone(),
            writer_tx.clone(),
            config.workspace.clone(),
            terminal.clone(),
            terminals.clone(),
        ));
        let mut session = Self {
            child,
            writer: writer_tx,
            shared,
            tasks: vec![writer, stderr_task, reader],
            stderr: captured,
        };
        // Drop the filtered environment: only the configured child sees it.
        session
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": terminal_enabled
                    },
                    "clientInfo": {"name": "ene", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let cwd = config
            .cwd
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
        let created = session
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let session_id = created["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?;
        session.shared.lock().await.session_id = session_id.into();
        Ok(session)
    }

    pub(crate) async fn prompt(&mut self, prompt: &[ContentBlock]) -> Result<Turn, Error> {
        let prompt = prompt
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => json!({"type": "text", "text": text.text}),
            })
            .collect::<Vec<_>>();
        let (rx, id) = {
            let mut state = self.shared.lock().await;
            if state.turn.is_some() || state.permission.is_some() {
                return Err(Error::new(ErrorKind::StateConflict));
            }
            let (tx, rx) = oneshot::channel();
            state.turn = Some(tx);
            let id = state.request_id();
            state.prompt_id = Some(id);
            (rx, id)
        };
        let session_id = self.session_id().await?;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": prompt}
        }))
        .await?;
        self.await_turn(rx).await
    }

    pub(crate) async fn respond(
        &mut self,
        request_id: &str,
        outcome: &RequestPermissionOutcome,
    ) -> Result<Turn, Error> {
        let (rpc_id, rx) = {
            let mut state = self.shared.lock().await;
            let permission = state
                .permission
                .take()
                .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
            if permission.request_id != request_id {
                state.permission = Some(permission);
                return Err(Error::new(ErrorKind::StateConflict));
            }
            // A terminal command is only accepted immediately after the user
            // selected an allow option for the agent's permission request.
            if let RequestPermissionOutcome::Selected { option_id } = outcome
                && permission.options.iter().any(|option| {
                    &option.option_id == option_id
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
            (permission.rpc_id, rx)
        };
        let outcome = serde_json::to_value(outcome).map_err(|_| Error::invalid())?;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {"outcome": outcome}
        }))
        .await?;
        self.await_turn(rx).await
    }

    pub(crate) async fn cancel(&mut self) -> Result<(), Error> {
        let session_id = self.session_id().await?;
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id}
        }))
        .await
    }

    pub(crate) async fn kill(mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
        for task in &self.tasks {
            task.abort();
        }
    }

    async fn session_id(&self) -> Result<String, Error> {
        let session_id = self.shared.lock().await.session_id.clone();
        if session_id.is_empty() {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        Ok(session_id)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let (id, rx) = {
            let mut state = self.shared.lock().await;
            let id = state.request_id();
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id, tx);
            (id, rx)
        };
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;
        timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| Error::new(ErrorKind::Unavailable))?
            .map_err(|_| Error::new(ErrorKind::Unavailable))?
            .map_err(|_| Error {
                kind: ErrorKind::ExternalFailure,
                reason: Some("acp_request_failed"),
            })
    }

    async fn send(&self, value: Value) -> Result<(), Error> {
        self.writer
            .send(value)
            .await
            .map_err(|_| Error::new(ErrorKind::Unavailable))
    }

    async fn await_turn(
        &mut self,
        rx: oneshot::Receiver<std::result::Result<Turn, String>>,
    ) -> Result<Turn, Error> {
        match timeout(TURN_TIMEOUT, rx).await {
            Ok(Ok(Ok(turn))) => Ok(turn),
            Ok(Ok(Err(reason))) => {
                let captured = self.stderr.lock().await;
                let stderr = String::from_utf8_lossy(&captured);
                eprintln!("ene-host acp: {reason}: {stderr}");
                drop(captured);
                Err(Error {
                    kind: ErrorKind::ExternalFailure,
                    reason: Some("acp_agent_failed"),
                })
            }
            Ok(Err(_)) => Err(Error::new(ErrorKind::Unavailable)),
            Err(_) => {
                let _ = self.cancel().await;
                Err(Error::new(ErrorKind::Unavailable))
            }
        }
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, captured: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0_u8; 1024];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut buffer).await {
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

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Mutex<State>>,
    writer: mpsc::Sender<Value>,
    workspace: Workspace,
    terminal: Option<Arc<TerminalLimits>>,
    terminals: Arc<Mutex<HashMap<String, Terminal>>>,
) {
    let context = ClientContext {
        shared: shared.clone(),
        writer: writer.clone(),
        workspace,
        terminal,
        terminals,
    };
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            fail(&shared, "invalid_json", &writer).await;
            return;
        };
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if let Some(id) = value.get("id").and_then(Value::as_u64) {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                handle_client_request(id, method, params, &context).await;
            } else if method == "session/update"
                && !accept_update(&shared, value.get("params").unwrap_or(&Value::Null)).await
            {
                fail(&shared, "invalid_update", &writer).await;
                return;
            }
            continue;
        }
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            let mut state = shared.lock().await;
            if state.prompt_id == Some(id) {
                state.prompt_id = None;
                let stop = value
                    .get("result")
                    .and_then(|result| result.get("stopReason"))
                    .and_then(Value::as_str)
                    .and_then(stop_reason)
                    .unwrap_or(StopReason::EndTurn);
                if let Some(turn) = state.turn.take() {
                    let updates = std::mem::take(&mut state.updates);
                    let _ = turn.send(Ok(Turn {
                        updates,
                        state: TurnState::Stopped(stop),
                    }));
                }
                continue;
            }
            if let Some(sender) = state.pending.remove(&id) {
                let result = match value.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("rpc error")
                        .to_owned()),
                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(result);
            }
        }
    }
    fail(&shared, "agent_exited", &writer).await;
}

async fn accept_update(shared: &Arc<Mutex<State>>, params: &Value) -> bool {
    let Some(update) = params.get("update") else {
        return false;
    };
    let Some(mapped) = map_update(update) else {
        // Updates outside the supported subset are ignored, as ACP clients do.
        return true;
    };
    let mut state = shared.lock().await;
    if state.session_id.is_empty() {
        return false;
    }
    if let Some(session) = params.get("sessionId").and_then(Value::as_str)
        && session != state.session_id
    {
        return false;
    }
    state.updates.push(mapped);
    state.updates.len() <= MAX_UPDATES
}

fn map_update(update: &Value) -> Option<SessionUpdate> {
    match update.get("sessionUpdate").and_then(Value::as_str)? {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let content = update.get("content")?;
            if content.get("type").and_then(Value::as_str) != Some("text") {
                return None;
            }
            let content = ContentBlock::text(content.get("text").and_then(Value::as_str)?);
            let chunk = ContentChunk {
                content,
                message_id: update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            Some(match update.get("sessionUpdate").and_then(Value::as_str) {
                Some("agent_thought_chunk") => SessionUpdate::AgentThoughtChunk(chunk),
                _ => SessionUpdate::AgentMessageChunk(chunk),
            })
        }
        "tool_call" => serde_json::from_value::<ToolCall>(update.clone())
            .ok()
            .map(SessionUpdate::ToolCall),
        "tool_call_update" => serde_json::from_value::<ToolCallUpdate>(update.clone())
            .ok()
            .map(SessionUpdate::ToolCallUpdate),
        "plan" => serde_json::from_value::<Plan>(update.clone())
            .ok()
            .map(SessionUpdate::Plan),
        _ => None,
    }
}

fn stop_reason(value: &str) -> Option<StopReason> {
    Some(match value {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "max_turn_requests" => StopReason::MaxTurnRequests,
        "refusal" => StopReason::Refusal,
        "cancelled" => StopReason::Cancelled,
        _ => return None,
    })
}

async fn handle_client_request(id: u64, method: &str, params: Value, context: &ClientContext) {
    let ClientContext {
        shared,
        writer,
        workspace,
        terminal,
        terminals,
    } = context.clone();
    match method {
        "session/request_permission" => {
            let Some(tool_call) = params
                .get("toolCall")
                .cloned()
                .and_then(|value| serde_json::from_value::<ToolCallUpdate>(value).ok())
            else {
                respond_error(&writer, id, -32602, "invalid tool call").await;
                return;
            };
            let options = params
                .get("options")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<PermissionOption>>(value).ok())
                .unwrap_or_default();
            let mut state = shared.lock().await;
            state.permission = Some(Permission {
                request_id: id.to_string(),
                rpc_id: id,
                options: options.clone(),
            });
            if let Some(turn) = state.turn.take() {
                let updates = std::mem::take(&mut state.updates);
                let _ = turn.send(Ok(Turn {
                    updates,
                    state: TurnState::Waiting(Box::new(Waiting {
                        request_id: id.to_string(),
                        tool_call,
                        options,
                    })),
                }));
            }
        }
        "terminal/create" => {
            let Some(policy) = terminal.as_deref() else {
                respond_error(&writer, id, -32601, "terminal is not enabled").await;
                return;
            };
            let approved = {
                let mut state = shared.lock().await;
                let approved = state.approved;
                state.approved = false;
                approved
            };
            if !approved {
                respond_error(&writer, id, -32000, "permission required").await;
                return;
            }
            let Some(command) = params.get("command").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "missing command").await;
                return;
            };
            if !command_allowed(policy, command) {
                respond_error(&writer, id, -32000, "command not allowed").await;
                return;
            }
            let args: Vec<String> = params
                .get("args")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let cwd = match params.get("cwd").and_then(Value::as_str) {
                Some(cwd) if !cwd.is_empty() => match workspace.directory(cwd) {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        respond_error(&writer, id, rpc_error(error), "invalid cwd").await;
                        return;
                    }
                },
                _ => workspace.root().to_path_buf(),
            };
            let mut envs = Vec::new();
            if let Some(values) = params.get("env").and_then(Value::as_array) {
                for value in values {
                    let (Some(name), Some(value)) = (
                        value.get("name").and_then(Value::as_str),
                        value.get("value").and_then(Value::as_str),
                    ) else {
                        respond_error(&writer, id, -32602, "invalid env").await;
                        return;
                    };
                    if name.is_empty()
                        || name.len() > 256
                        || name.contains(['=', '\0'])
                        || name.chars().any(char::is_control)
                    {
                        respond_error(&writer, id, -32602, "invalid env name").await;
                        return;
                    }
                    envs.push((name.to_owned(), value.to_owned()));
                }
            }
            let limit = params
                .get("outputByteLimit")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(policy.max_output)
                .clamp(1, policy.max_output);
            let mut command = Command::new(command);
            command
                .args(&args)
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
            for (name, value) in envs {
                command.env(name, value);
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(_) => {
                    respond_error(&writer, id, -32000, "failed to start command").await;
                    return;
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
            let readers = Arc::new(Mutex::new(readers));
            let terminal_id = uuid::Uuid::new_v4().to_string();
            terminals.lock().await.insert(
                terminal_id.clone(),
                Terminal {
                    child: child.clone(),
                    output,
                    truncated,
                    exit,
                    readers,
                },
            );
            let seconds = policy.max_seconds;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                let mut child = child.lock().await;
                let _ = child.start_kill();
            });
            respond(&writer, id, json!({"terminalId": terminal_id})).await;
        }
        "terminal/output" => {
            let Some(terminal_id) = params.get("terminalId").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "missing terminal").await;
                return;
            };
            let (output, truncated, exited) = {
                let terminals = terminals.lock().await;
                let Some(terminal) = terminals.get(terminal_id) else {
                    respond_error(&writer, id, -32602, "unknown terminal").await;
                    return;
                };
                (
                    terminal.output.lock().await.clone(),
                    terminal.truncated.load(Ordering::Relaxed),
                    *terminal.exit.lock().await,
                )
            };
            let mut result = json!({
                "output": String::from_utf8_lossy(&output),
                "truncated": truncated,
            });
            if let Some(code) = exited {
                result["exitStatus"] =
                    code.map_or_else(|| json!({}), |code| json!({"exitCode": code}));
            }
            respond(&writer, id, result).await;
        }
        "terminal/wait_for_exit" => {
            let Some(terminal_id) = params.get("terminalId").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "missing terminal").await;
                return;
            };
            let (child, exit, readers) = {
                let terminals = terminals.lock().await;
                let Some(terminal) = terminals.get(terminal_id) else {
                    respond_error(&writer, id, -32602, "unknown terminal").await;
                    return;
                };
                (
                    terminal.child.clone(),
                    terminal.exit.clone(),
                    terminal.readers.clone(),
                )
            };
            let status = child.lock().await.wait().await;
            let code = status.ok().and_then(|status| status.code());
            *exit.lock().await = Some(code);
            let readers = std::mem::take(&mut *readers.lock().await);
            for reader in readers {
                let _ = reader.await;
            }
            respond(
                &writer,
                id,
                json!({"exitStatus": code.map_or_else(|| json!({}), |code| json!({"exitCode": code}))}),
            )
            .await;
        }
        "terminal/kill" => {
            let Some(terminal_id) = params.get("terminalId").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "missing terminal").await;
                return;
            };
            let child = {
                let terminals = terminals.lock().await;
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.child.clone())
            };
            if let Some(child) = child {
                let _ = child.lock().await.start_kill();
            }
            respond(&writer, id, Value::Null).await;
        }
        "terminal/release" => {
            let Some(terminal_id) = params.get("terminalId").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "missing terminal").await;
                return;
            };
            let terminal = terminals.lock().await.remove(terminal_id);
            if let Some(terminal) = terminal {
                let _ = terminal.child.lock().await.start_kill();
            }
            respond(&writer, id, Value::Null).await;
        }
        "fs/read_text_file" => {
            let Some(path) = params.get("path").and_then(Value::as_str) else {
                respond_error(&writer, id, -32602, "invalid path").await;
                return;
            };
            let line = params.get("line").and_then(Value::as_u64).map(|v| v as u32);
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            match relative_to(&workspace, path).and_then(|path| workspace.read(&path, line, limit))
            {
                Ok(read) => respond(&writer, id, json!({"content": read.content})).await,
                Err(error) => respond_error(&writer, id, rpc_error(error), "read failed").await,
            }
        }
        "fs/write_text_file" => {
            let (Some(path), Some(content)) = (
                params.get("path").and_then(Value::as_str),
                params.get("content").and_then(Value::as_str),
            ) else {
                respond_error(&writer, id, -32602, "invalid write").await;
                return;
            };
            match relative_to(&workspace, path).and_then(|path| workspace.write(&path, content)) {
                Ok(()) => respond(&writer, id, Value::Null).await,
                Err(error) => respond_error(&writer, id, rpc_error(error), "write failed").await,
            }
        }
        _ => respond_error(&writer, id, -32601, "method not supported").await,
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

fn relative_to(workspace: &Workspace, path: &str) -> Result<String, Error> {
    let path = Path::new(path);
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

fn rpc_error(error: Error) -> i64 {
    match error.kind {
        ErrorKind::PermissionRequired => -32000,
        ErrorKind::NotFound => -32602,
        ErrorKind::InvalidRequest => -32602,
        _ => -32603,
    }
}

async fn respond(writer: &mpsc::Sender<Value>, id: u64, result: Value) {
    let _ = writer
        .send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
        .await;
}

async fn respond_error(writer: &mpsc::Sender<Value>, id: u64, code: i64, message: &str) {
    let _ = writer
        .send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }))
        .await;
}

async fn fail(shared: &Arc<Mutex<State>>, reason: &str, writer: &mpsc::Sender<Value>) {
    let _ = writer; // keep the writer alive for pending responses
    let mut state = shared.lock().await;
    if let Some(turn) = state.turn.take() {
        let _ = turn.send(Err(reason.into()));
    }
    state.pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let turn = session.prompt(&[ContentBlock::text("go")]).await.unwrap();
        assert!(matches!(turn.state, TurnState::Waiting(_)));
        let TurnState::Waiting(waiting) = turn.state else {
            panic!("expected permission");
        };
        let request_id = waiting.request_id;
        let options = waiting.options;
        assert_eq!(options[0].option_id, "allow");
        assert!(turn.updates.iter().any(|update| matches!(
            update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "hello")
        )));
        let turn = session
            .respond(
                &request_id,
                &RequestPermissionOutcome::Selected {
                    option_id: "allow".into(),
                },
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
}
