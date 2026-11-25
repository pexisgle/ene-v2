use ene_avatar_ipc::{AvatarEvent, AvatarMessage, AvatarStatus, CharacterConfig, Command, Request};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

pub const AVATAR_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_EVENT_QUEUE: usize = 256;

#[derive(Clone, Debug, thiserror::Error)]
pub enum AvatarError {
    #[error("{0}")]
    Encode(String),
    #[error("{path}: {error}")]
    Spawn { path: String, error: String },
    #[error("the renderer has no stdin")]
    NoStdin,
    #[error("the renderer has no stdout")]
    NoStdout,
    #[error("the renderer is no longer healthy")]
    Unhealthy,
    #[error("the renderer input closed")]
    InputClosed,
    #[error("the renderer input closed: {0}")]
    WriteFailed(String),
    #[error("the renderer exited")]
    Exited,
    #[error("the renderer timed out answering an IPC command")]
    Timeout,
    #[error("the renderer did not load the model")]
    ModelLoadFailed,
    #[error("the renderer sent invalid JSON: {0}")]
    InvalidJson(String),
    #[error("the renderer output failed: {0}")]
    OutputFailed(String),
    #[error("{0}")]
    Renderer(String),
}

impl From<ene_avatar_ipc::EncodeError> for AvatarError {
    fn from(error: ene_avatar_ipc::EncodeError) -> Self {
        Self::Encode(error.to_string())
    }
}

pub(super) fn lip_sync_line(character: &str, rms: f32) -> Result<String, AvatarError> {
    Ok(Request {
        id: None,
        command: Command::LipSync {
            character: character.to_owned(),
            rms: rms.clamp(0., 1.),
        },
    }
    .encode_line()?)
}

pub(super) enum WriteItem {
    Request { id: u64, line: String },
    OneWay(String),
}
pub(super) type PendingReplies =
    Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, AvatarError>>>>>;

/// The renderer process and non-blocking async channels to its stdio workers.
pub struct Avatar {
    child: Option<Child>,
    pub(super) write_tx: Option<mpsc::Sender<WriteItem>>,
    events_rx: mpsc::UnboundedReceiver<AvatarEvent>,
    pending: PendingReplies,
    pub(super) events: VecDeque<AvatarEvent>,
    next_id: u64,
    /// Shared with both stdio workers, so a renderer that dies while idle is
    /// reported unhealthy before the next call notices.
    pub(super) healthy: Arc<AtomicBool>,
    pub(super) shown: HashMap<String, CharacterConfig>,
    pub(super) base_motion: HashMap<String, String>,
    pub(super) last_motion: HashMap<String, String>,
}

impl Avatar {
    #[cfg(windows)]
    pub(crate) fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Spawns the renderer. `program` is the `ene-avatar` binary; the caller
    /// resolves it so a missing binary is an error before any model loads.
    pub fn spawn(program: &Path) -> Result<Self, AvatarError> {
        let mut command = std::process::Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|error| AvatarError::Spawn {
            path: program.display().to_string(),
            error: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or(AvatarError::NoStdin)?;
        let stdout = child.stdout.take().ok_or(AvatarError::NoStdout)?;
        let (write_tx, write_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let pending: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
        let healthy = Arc::new(AtomicBool::new(true));
        let writer_pending = Arc::clone(&pending);
        let writer_healthy = Arc::clone(&healthy);
        thread::spawn(move || write_loop(stdin, write_rx, writer_pending, &writer_healthy));
        let reader_pending = Arc::clone(&pending);
        let reader_healthy = Arc::clone(&healthy);
        thread::spawn(move || read_loop(stdout, events_tx, reader_pending, &reader_healthy));
        Ok(Self {
            child: Some(child),
            write_tx: Some(write_tx),
            events_rx,
            pending,
            events: VecDeque::new(),
            next_id: 1,
            healthy,
            shown: HashMap::new(),
            base_motion: HashMap::new(),
            last_motion: HashMap::new(),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    fn push_event(&mut self, event: AvatarEvent) {
        if self.events.len() >= MAX_EVENT_QUEUE {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub(super) fn drain_incoming_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.push_event(event);
        }
    }

    /// Sends one command and awaits its reply with a bounded timeout so a stalled
    /// renderer never freezes the Tokio thread.
    pub(super) async fn call(&mut self, command: Command) -> Result<Value, AvatarError> {
        if !self.is_healthy() {
            return Err(AvatarError::Unhealthy);
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = Request {
            id: Some(id),
            command,
        }
        .encode_line()?;
        let Some(write_tx) = self.write_tx.clone() else {
            return Err(AvatarError::InputClosed);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, reply_tx);
        if write_tx
            .send(WriteItem::Request { id, line })
            .await
            .is_err()
        {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            self.healthy.store(false, Ordering::Release);
            return Err(AvatarError::InputClosed);
        }
        match tokio::time::timeout(AVATAR_CALL_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.healthy.store(false, Ordering::Release);
                Err(AvatarError::Exited)
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                self.healthy.store(false, Ordering::Release);
                if let Some(child) = self.child.as_mut()
                    && let Err(error) = child.kill()
                {
                    tracing::debug!(%error, "the avatar child could not be killed");
                }
                Err(AvatarError::Timeout)
            }
        }
    }

    /// Drains all buffered notifications without dropping click or state events.
    pub fn notifications(&mut self) -> Vec<AvatarEvent> {
        self.drain_incoming_events();
        self.events.drain(..).collect()
    }

    /// Waits for the renderer to publish a terminal `state` event without
    /// discarding unrelated notifications or blocking the async runtime.
    pub async fn await_state(
        &mut self,
        character: &str,
        wait: Duration,
    ) -> Result<(), AvatarError> {
        let deadline = tokio::time::Instant::now() + wait;
        self.drain_incoming_events();
        if let Some(result) = self.take_terminal_state(character) {
            return result;
        }
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            match tokio::time::timeout(deadline - now, self.events_rx.recv()).await {
                Ok(Some(event)) => match &event {
                    AvatarEvent::State {
                        character: got,
                        status,
                        error,
                    } if got == character => match status {
                        AvatarStatus::Ready | AvatarStatus::Suspended => {
                            self.push_event(event);
                            return Ok(());
                        }
                        AvatarStatus::Error => {
                            let err = error
                                .clone()
                                .map_or(AvatarError::ModelLoadFailed, AvatarError::Renderer);
                            self.push_event(event);
                            return Err(err);
                        }
                        AvatarStatus::Loading => {}
                    },
                    _ => self.push_event(event),
                },
                Ok(None) | Err(_) => return Ok(()),
            }
        }
    }

    fn take_terminal_state(&mut self, character: &str) -> Option<Result<(), AvatarError>> {
        let pos = self.events.iter().position(|event| {
            matches!(
                event,
                AvatarEvent::State {
                    character: got,
                    status: AvatarStatus::Ready | AvatarStatus::Suspended | AvatarStatus::Error,
                    ..
                } if got == character
            )
        })?;
        // Consume the event: leaving it queued returned the same terminal state
        // on every later `await_state`, so a character that recovered still
        // reported its old error.
        let event = self.events.remove(pos)?;
        match &event {
            AvatarEvent::State {
                status: AvatarStatus::Ready | AvatarStatus::Suspended,
                ..
            } => Some(Ok(())),
            AvatarEvent::State {
                status: AvatarStatus::Error,
                error,
                ..
            } => Some(Err(error
                .clone()
                .map_or(AvatarError::ModelLoadFailed, AvatarError::Renderer))),
            _ => None,
        }
    }

    /// Sends `quit` without waiting indefinitely; kills the child if it does
    /// not exit within the bounded shutdown budget.
    pub fn shutdown(mut self) {
        if let Some(write_tx) = self.write_tx.take()
            && let Ok(line) = (Request {
                id: Some(self.next_id),
                command: Command::Quit {},
            })
            .encode_line()
        {
            let _queued = write_tx.try_send(WriteItem::OneWay(line));
        }
        // Taking the child marks it reaped, so `Drop` cannot kill a pid that
        // the OS may already have reused.
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + AVATAR_SHUTDOWN_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(15));
                }
                _ => {
                    reap(&mut child);
                    return;
                }
            }
        }
    }
}

/// Channels a test uses to stand in for the renderer process. Constructed by
/// `Avatar::for_test`, driven by the test module.
#[cfg(test)]
pub(super) struct TestRenderer {
    pub(super) requests: mpsc::Receiver<WriteItem>,
    pub(super) pending: PendingReplies,
    pub(super) events: mpsc::UnboundedSender<AvatarEvent>,
}

#[cfg(test)]
impl Avatar {
    /// Builds an `Avatar` with no child process, so a test can answer its own
    /// requests and publish its own events.
    pub(super) fn for_test(shown: HashMap<String, CharacterConfig>) -> (Self, TestRenderer) {
        let (write_tx, requests) = mpsc::channel(64);
        let (events, events_rx) = mpsc::unbounded_channel();
        let pending: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
        let avatar = Self {
            child: None,
            write_tx: Some(write_tx),
            events_rx,
            pending: Arc::clone(&pending),
            events: VecDeque::new(),
            next_id: 1,
            healthy: Arc::new(AtomicBool::new(true)),
            shown,
            base_motion: HashMap::new(),
            last_motion: HashMap::new(),
        };
        (
            avatar,
            TestRenderer {
                requests,
                pending,
                events,
            },
        )
    }
}

/// Kills a still-running renderer and reaps it. Errors are debug-only because
/// a renderer that already exited makes both calls fail harmlessly.
fn reap(child: &mut Child) {
    if let Err(error) = child.kill() {
        tracing::debug!(%error, "the avatar child could not be killed");
    }
    if let Err(error) = child.wait() {
        tracing::debug!(%error, "the avatar child could not be reaped");
    }
}

impl Drop for Avatar {
    fn drop(&mut self) {
        self.write_tx.take();
        if let Some(mut child) = self.child.take() {
            reap(&mut child);
        }
    }
}

/// Runs the writer until the pipe closes, then marks the renderer unhealthy. A
/// stdio worker only returns when its pipe is gone, so no later call can
/// succeed.
fn write_loop(
    mut stdin: ChildStdin,
    mut rx: mpsc::Receiver<WriteItem>,
    pending: PendingReplies,
    healthy: &AtomicBool,
) {
    while let Some(item) = rx.blocking_recv() {
        let (id, line) = match item {
            WriteItem::Request { id, line } => (Some(id), line),
            WriteItem::OneWay(line) => (None, line),
        };
        if let Err(error) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
            let reason = AvatarError::WriteFailed(error.to_string());
            if let Some(id) = id
                && let Some(tx) = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
            {
                let _replied = tx.send(Err(reason));
            }
            break;
        }
    }
    healthy.store(false, Ordering::Release);
}

/// Reads renderer output until stdin closes, then marks the renderer
/// unhealthy, so one that dies while idle is reported before the next call
/// notices the closed pipe.
pub(super) fn read_loop(
    stdout: impl std::io::Read,
    events_tx: mpsc::UnboundedSender<AvatarEvent>,
    pending: PendingReplies,
    healthy: &AtomicBool,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut fail_reason = AvatarError::Exited;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => match serde_json::from_str::<AvatarMessage>(&line) {
                Ok(AvatarMessage::Reply { id, ok, error }) => {
                    if let Some(tx) = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                    {
                        let _discarded = tx.send(match error {
                            Some(error) => Err(AvatarError::Renderer(error)),
                            None => Ok(ok),
                        });
                    }
                }
                Ok(AvatarMessage::Event(event)) => {
                    let _emitted = events_tx.send(event);
                }
                Err(error) => {
                    fail_reason = AvatarError::InvalidJson(error.to_string());
                    break;
                }
            },
            Err(error) => {
                fail_reason = AvatarError::OutputFailed(error.to_string());
                break;
            }
        }
    }
    let waiters: Vec<_> = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    for tx in waiters {
        let _replied = tx.send(Err(fail_reason.clone()));
    }
    healthy.store(false, Ordering::Release);
}
