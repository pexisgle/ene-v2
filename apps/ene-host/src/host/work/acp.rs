//! The external ACP backend (tech-stack.md §G): one process per WorkSession,
//! one `session/prompt` turn per Run. Commands go to [`AcpHandle`]; [`AcpEvents`]
//! carries results back to [`pump`], which settles them like the rig backend.
use crate::host::{
    db::{Store, truncate},
    error::HostError,
    notify::Notifier,
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
use crossbeam_channel::{Receiver, Sender, unbounded};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rig_core::completion::Message;
use rig_ecs::prelude::TurnOutcome;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const KILL_TIMEOUT: Duration = Duration::from_secs(5);

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
    commands: UnboundedSender<Command>,
}

impl AcpHandle {
    pub(crate) fn pid(&self) -> Vec<u32> {
        let pid = self.pid.load(Ordering::SeqCst);
        (pid != 0).then_some(pid).into_iter().collect()
    }

    pub(crate) fn prompt(&self, run_id: &str, text: &str) -> bool {
        self.commands
            .send(Command::Prompt {
                run_id: run_id.to_owned(),
                text: text.to_owned(),
            })
            .is_ok()
    }

    pub(crate) fn cancel(&self, run_id: &str) -> bool {
        self.commands
            .send(Command::Cancel {
                run_id: run_id.to_owned(),
            })
            .is_ok()
    }

    pub(crate) fn end(&self) -> bool {
        self.commands.send(Command::End).is_ok()
    }
}

#[derive(Resource, Default)]
pub(crate) struct AcpBackends(pub(crate) HashMap<String, AcpHandle>);

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
    fn sender(&self) -> Sender<AcpEvent> {
        self.sender.clone()
    }

    fn drain(&self) -> Vec<AcpEvent> {
        self.receiver.try_iter().collect()
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
    which::which(command).map_err(|_| {
        HostError::unavailable(format!("ACP command `{command}` was not found in PATH"))
    })?;
    let cwd = std::env::current_dir()
        .map_err(|error| HostError::unavailable(format!("ACP working directory: {error}")))?;
    let (program, args, notice) = launch(
        command,
        &connection.args,
        &cwd,
        &connection.endpoint.options,
    );
    if let Some(notice) = notice {
        let store = world.resource::<Store>().clone();
        if let Ok(Some(session)) = store.work_session(work_id) {
            world.resource_mut::<Notifier>().notice(
                Some(&session.companion_id),
                "acp_sandbox_unavailable",
                &truncate(&notice, 500),
            );
        }
    }
    let events = world.resource::<AcpEvents>().sender();
    let pid = Arc::new(AtomicU32::new(0));
    let (commands, receiver) = unbounded_channel();
    let handle = AcpHandle {
        pid: Arc::clone(&pid),
        commands,
    };
    runtime()?.spawn(run_backend(
        work_id.to_owned(),
        program,
        args,
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
    for event in world.resource::<AcpEvents>().drain() {
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

fn closed(work_id: String, error: String) -> AcpEvent {
    AcpEvent::Closed {
        work_id,
        run_id: None,
        error: Some(error),
        confirmed: true,
        pids: Vec::new(),
    }
}

async fn run_backend(
    work_id: String,
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    events: Sender<AcpEvent>,
    pid: Arc<AtomicU32>,
    commands: UnboundedReceiver<Command>,
) {
    let mut wrap = CommandWrap::with_new(&program, |cmd| {
        cmd.args(&args)
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
            let _ = events.send(closed(
                work_id,
                format!("the ACP agent could not start: {error}"),
            ));
            return;
        }
    };
    if let Some(id) = child.id() {
        pid.store(id, Ordering::SeqCst);
    }
    let (Some(stdin), Some(stdout)) = (child.stdin().take(), child.stdout().take()) else {
        let _ = events.send(closed(
            work_id,
            "the ACP agent has no standard input/output".to_owned(),
        ));
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
    let (confirmed, pids) = terminate(&mut child, KILL_TIMEOUT).await;
    let _ = events.send(AcpEvent::Closed {
        work_id,
        run_id: shared.run_id(),
        error: result.err().map(|error| error.to_string()),
        confirmed,
        pids,
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
                            let _ = events.send(AcpEvent::Ready { run_id: run_id.clone() });
                        }
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

async fn serve(
    cx: &ConnectionTo<Agent>,
    session_id: &SessionId,
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
                let _ = events.send(AcpEvent::Finished { run_id, outcome });
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
        let mut slot = lock(&self.state);
        slot.run_id = Some(run_id.to_owned());
        slot.summary.clear();
    }

    fn run_id(&self) -> Option<String> {
        lock(&self.state).run_id.clone()
    }

    fn append(&self, text: &str) {
        let mut slot = lock(&self.state);
        if slot.summary.chars().count() < 8_000 {
            slot.summary.push_str(text);
        }
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

// === Process isolation ===================================================

fn launch(
    command: &str,
    args: &[String],
    cwd: &Path,
    options: &str,
) -> (PathBuf, Vec<String>, Option<String>) {
    let requested = sandbox_mode(options);
    if requested.as_deref() == Some("none") {
        let reason = "ACP接続の `sandbox` オプションにより、子プロセスは非隔離で起動します。";
        return (
            PathBuf::from(command),
            args.to_vec(),
            Some(reason.to_owned()),
        );
    }
    #[cfg(target_os = "linux")]
    if let (Some(program), Some(cwd)) = (which::which("bwrap").ok(), cwd.to_str()) {
        let mut wrapped: Vec<String> = ["--die-with-parent", "--unshare-net", "--unshare-pid"]
            .map(str::to_owned)
            .to_vec();
        wrapped.extend(["--bind", "/", "/", "--proc", "/proc"].map(str::to_owned));
        wrapped.extend(["--dev-bind", "/dev", "/dev", "--chdir"].map(str::to_owned));
        wrapped.extend([cwd, "--", command].map(str::to_owned));
        wrapped.extend(args.iter().cloned());
        return (program, wrapped, None);
    }
    let reason = if requested.as_deref() == Some("bwrap") {
        "`sandbox: bwrap` が指定されましたが bwrap が見つからないため、ACP子プロセスは非隔離で起動します。"
    } else {
        "この環境では bwrap を利用できないため、ACP子プロセスは非隔離で起動します。"
    };
    (
        PathBuf::from(command),
        args.to_vec(),
        Some(reason.to_owned()),
    )
}

fn sandbox_mode(options: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(options).ok()?;
    let mode = value.get("sandbox")?.as_str()?.trim().to_ascii_lowercase();
    (!mode.is_empty()).then_some(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicitly_disabled_sandbox_reports_itself_as_non_isolated() {
        let args = ["--stdio".to_owned()];
        let (program, args, notice) = launch(
            "opencode",
            &args,
            Path::new("/tmp"),
            r#"{"sandbox":"none"}"#,
        );
        assert_eq!(program, PathBuf::from("opencode"));
        assert_eq!(args, ["--stdio".to_owned()]);
        assert!(notice.is_some_and(|notice| notice.contains("非隔離")));
    }
}
