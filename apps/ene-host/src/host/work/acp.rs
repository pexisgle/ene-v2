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
    ffi::OsString,
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

/// Longest a forced kill may take to be reaped; `work::FORCE_GRACE` is the
/// wait that must outlast it before a stop counts as unconfirmed.
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
    )
    .map_err(HostError::Unavailable)?;
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
    let env = child_env(&connection.endpoint.options, |name: &str| {
        std::env::var_os(name)
    });
    runtime()?.spawn(run_backend(
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
    events: Sender<AcpEvent>,
    pid: Arc<AtomicU32>,
    commands: UnboundedReceiver<Command>,
) {
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
    let Some((session_id, first)) = open(&cx, &cwd, &shared, &mut commands).await? else {
        return Ok(());
    };
    let mut ready = false;
    if let Some((run_id, text)) = first {
        announce(&mut ready, &events, &run_id);
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
                        announce(&mut ready, &events, &run_id);
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
    commands: &mut UnboundedReceiver<Command>,
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

fn announce(ready: &mut bool, events: &Sender<AcpEvent>, run_id: &str) {
    if !*ready {
        *ready = true;
        let _ = events.send(AcpEvent::Ready {
            run_id: run_id.to_owned(),
        });
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

// === Process isolation ===================================================

fn launch(
    command: &str,
    args: &[String],
    cwd: &Path,
    options: &str,
) -> Result<(PathBuf, Vec<String>, Option<String>), String> {
    let requested = sandbox_mode(options);
    if requested.as_deref() == Some("none") {
        let notice = "ACP接続の `sandbox: none` オプションにより、子プロセスは非隔離で起動します。";
        return Ok((
            PathBuf::from(command),
            args.to_vec(),
            Some(notice.to_owned()),
        ));
    }
    plan(
        command,
        args,
        cwd,
        requested.as_deref(),
        which::which("bwrap").ok().as_deref(),
    )
}

/// Builds the bwrap command line. The system stays read-only (`--ro-bind / /`)
/// and only the working directory is writable; without bwrap the child cannot
/// be isolated, so the launch is refused unless the connection already opted
/// out with `sandbox: none`.
fn plan(
    command: &str,
    args: &[String],
    cwd: &Path,
    requested: Option<&str>,
    bwrap: Option<&Path>,
) -> Result<(PathBuf, Vec<String>, Option<String>), String> {
    let Some(bwrap) = bwrap else {
        let requested = match requested {
            Some(mode) => format!("接続オプション `sandbox: {mode}` が指定されましたが、"),
            None => String::new(),
        };
        return Err(format!(
            "{requested}この環境では bwrap が見つからず、ACP子プロセスを隔離できません。\
             非隔離での実行を明示的に承認するには、接続オプションに `sandbox: none` を設定してください。"
        ));
    };
    let cwd = cwd
        .to_str()
        .ok_or_else(|| "ACPの作業ディレクトリをUTF-8で表せません".to_owned())?;
    let mut wrapped: Vec<String> = ["--die-with-parent", "--unshare-net", "--unshare-pid"]
        .map(str::to_owned)
        .to_vec();
    wrapped.extend(["--ro-bind", "/", "/", "--bind", cwd, cwd].map(str::to_owned));
    wrapped.extend(["--proc", "/proc", "--dev", "/dev", "--chdir", cwd].map(str::to_owned));
    wrapped.push("--".to_owned());
    wrapped.push(command.to_owned());
    wrapped.extend(args.iter().cloned());
    Ok((bwrap.to_path_buf(), wrapped, None))
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
        )
        .unwrap();
        assert_eq!(program, PathBuf::from("opencode"));
        assert_eq!(args, ["--stdio".to_owned()]);
        assert!(notice.is_some_and(|notice| notice.contains("非隔離")));
    }

    #[test]
    fn a_sandboxed_agent_reads_the_system_and_writes_only_the_work_directory() {
        let args = ["--stdio".to_owned()];
        let (program, wrapped, notice) = plan(
            "opencode",
            &args,
            Path::new("/work/project"),
            None,
            Some(Path::new("/usr/bin/bwrap")),
        )
        .unwrap();
        assert_eq!(program, PathBuf::from("/usr/bin/bwrap"));
        assert!(notice.is_none());
        let wrapped: Vec<&str> = wrapped.iter().map(String::as_str).collect();

        assert!(
            wrapped
                .windows(3)
                .any(|bind| bind[0] == "--ro-bind" && bind[1] == "/" && bind[2] == "/")
        );
        let bind = wrapped
            .iter()
            .position(|arg| *arg == "--bind")
            .expect("a writable bind");
        assert_eq!(wrapped[bind + 1], "/work/project");
        assert_eq!(wrapped[bind + 2], "/work/project");
        for flag in [
            "--unshare-net",
            "--unshare-pid",
            "--die-with-parent",
            "--proc",
            "--dev",
        ] {
            assert!(
                wrapped.contains(&flag),
                "{flag} is missing from {wrapped:?}"
            );
        }
        assert!(
            !wrapped
                .windows(3)
                .any(|bind| bind[0] == "--bind" && bind[1] == "/" && bind[2] == "/"),
            "the root must not be writable: {wrapped:?}"
        );
        assert_eq!(wrapped.last().copied(), Some("--stdio"));
    }

    #[test]
    fn an_agent_that_cannot_be_isolated_is_refused_without_the_explicit_opt_out() {
        let args = ["--stdio".to_owned()];
        let error = plan("opencode", &args, Path::new("/work/project"), None, None).unwrap_err();
        assert!(error.contains("bwrap"), "{error}");
        assert!(error.contains("隔離"), "{error}");
        assert!(error.contains("sandbox: none"), "{error}");

        let explicit = plan(
            "opencode",
            &args,
            Path::new("/work/project"),
            Some("bwrap"),
            None,
        )
        .unwrap_err();
        assert!(explicit.contains("sandbox: bwrap"), "{explicit}");
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
        world
            .resource::<AcpEvents>()
            .sender()
            .send(AcpEvent::Update {
                work_id: "w1".into(),
                run_id: "run-a".into(),
                text: "late progress".into(),
            })
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
