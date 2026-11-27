use ene_avatar_ipc::{
    AvatarEvent, AvatarMessage, AvatarStatus, CharacterConfig, Command, LAYOUT_FILE_ENV,
    RendererFailure, Request,
};
use futures_util::StreamExt as _;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    process::Command as Process,
    sync::{mpsc, oneshot},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    task::AbortOnDropHandle,
};

pub const AVATAR_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_EVENT_QUEUE: usize = 256;
const MAX_RENDERER_LINE: usize = 1024 * 1024;

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
    #[error("the character is not displayed by the renderer")]
    UnknownCharacter,
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

type Reply = oneshot::Sender<Result<(), AvatarError>>;

pub(super) enum WriteItem {
    Request { id: u64, line: String, reply: Reply },
    OneWay(String),
}

pub(super) enum ModelState {
    Loading,
    Ready,
    Suspended,
    Failed(Option<String>),
}

pub(super) struct ShownCharacter {
    pub(super) config: CharacterConfig,
    pub(super) state: ModelState,
    pub(super) base_motion: Option<String>,
    pub(super) last_motion: Option<String>,
    pub(super) motion_request: Option<u64>,
}

impl ShownCharacter {
    pub(super) fn new(config: CharacterConfig, state: ModelState) -> Self {
        Self {
            config,
            state,
            base_motion: None,
            last_motion: None,
            motion_request: None,
        }
    }
}

/// Owns one renderer subprocess and its bounded JSON Lines connection.
pub struct Avatar {
    #[cfg(windows)]
    pid: u32,
    started: Instant,
    worker: Option<AbortOnDropHandle<()>>,
    pub(super) write_tx: mpsc::Sender<WriteItem>,
    events_rx: mpsc::Receiver<AvatarEvent>,
    pub(super) events: VecDeque<AvatarEvent>,
    next_id: u64,
    pub(super) shown: HashMap<String, ShownCharacter>,
}

impl Avatar {
    #[cfg(windows)]
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub fn spawn(program: &Path, client_id: &str) -> Result<Self, AvatarError> {
        let mut command = Process::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .env_remove(LAYOUT_FILE_ENV);
        if let Some(dirs) = directories::ProjectDirs::from("", "", "ene") {
            let filename = format!("{}.json", hex::encode(Sha256::digest(client_id)));
            let layout = dirs.data_local_dir().join("avatar-layouts").join(filename);
            command.env(LAYOUT_FILE_ENV, layout);
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command.spawn().map_err(|error| AvatarError::Spawn {
            path: program.display().to_string(),
            error: error.to_string(),
        })?;
        #[cfg(windows)]
        let pid = child.id().unwrap_or_default();
        let stdin = child.stdin.take().ok_or(AvatarError::NoStdin)?;
        let stdout = child.stdout.take().ok_or(AvatarError::NoStdout)?;
        let (write_tx, requests) = mpsc::channel(64);
        let (events, events_rx) = mpsc::channel(MAX_EVENT_QUEUE);
        let worker = AbortOnDropHandle::new(tokio::spawn(async move {
            serve_io(stdin, stdout, requests, events).await;
            let _exited = child.wait().await;
        }));
        Ok(Self {
            #[cfg(windows)]
            pid,
            started: Instant::now(),
            worker: Some(worker),
            write_tx,
            events_rx,
            events: VecDeque::new(),
            next_id: 1,
            shown: HashMap::new(),
        })
    }

    pub fn is_healthy(&self) -> bool {
        !self.write_tx.is_closed()
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    fn push_event(&mut self, event: AvatarEvent) {
        if let AvatarEvent::State {
            character,
            status,
            error,
        } = &event
            && let Some(shown) = self.shown.get_mut(character)
        {
            shown.state = match status {
                AvatarStatus::Loading => ModelState::Loading,
                AvatarStatus::Ready => ModelState::Ready,
                AvatarStatus::Suspended => ModelState::Suspended,
                AvatarStatus::Error => ModelState::Failed(error.clone()),
            };
        }
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

    /// The timeout covers queueing, pipe writes and the renderer reply.
    pub(super) async fn call(&mut self, command: Command) -> Result<u64, AvatarError> {
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
        let (reply, answer) = oneshot::channel();
        let exchange = async {
            self.write_tx
                .send(WriteItem::Request { id, line, reply })
                .await
                .map_err(|_| AvatarError::InputClosed)?;
            answer.await.map_err(|_| AvatarError::Exited)?
        };
        match tokio::time::timeout(AVATAR_CALL_TIMEOUT, exchange).await {
            Ok(result) => result.map(|()| id),
            Err(_) => {
                if let Some(worker) = &self.worker {
                    worker.abort();
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

    /// Requests graceful shutdown, then cancels the child owner on timeout.
    pub async fn shutdown(mut self) {
        if let Ok(line) = (Request {
            id: None,
            command: Command::Quit {},
        })
        .encode_line()
        {
            let _queued = self.write_tx.try_send(WriteItem::OneWay(line));
        }
        if let Some(mut worker) = self.worker.take() {
            let _exited = tokio::time::timeout(AVATAR_SHUTDOWN_TIMEOUT, &mut worker).await;
        }
    }
}

#[cfg(test)]
pub(super) struct TestRenderer {
    pub(super) requests: mpsc::Receiver<WriteItem>,
    pub(super) events: mpsc::Sender<AvatarEvent>,
}

#[cfg(test)]
impl Avatar {
    pub(super) fn for_test(shown: HashMap<String, CharacterConfig>) -> (Self, TestRenderer) {
        let (write_tx, requests) = mpsc::channel(64);
        let (events, events_rx) = mpsc::channel(MAX_EVENT_QUEUE);
        let avatar = Self {
            #[cfg(windows)]
            pid: 0,
            started: Instant::now(),
            worker: None,
            write_tx,
            events_rx,
            events: VecDeque::new(),
            next_id: 1,
            shown: shown
                .into_iter()
                .map(|(id, config)| (id, ShownCharacter::new(config, ModelState::Ready)))
                .collect(),
        };
        (avatar, TestRenderer { requests, events })
    }
}

type InFlight = Option<(u64, Reply)>;

pub(super) async fn serve_io(
    stdin: impl AsyncWrite + Unpin,
    stdout: impl AsyncRead + Unpin,
    requests: mpsc::Receiver<WriteItem>,
    events: mpsc::Sender<AvatarEvent>,
) {
    let mut pending = None;
    if let Err(error) = exchange(stdin, stdout, requests, events, &mut pending).await {
        tracing::debug!(%error, "the avatar renderer disconnected");
        if let Some((_, reply)) = pending {
            let _replied = reply.send(Err(error));
        }
    }
}

async fn exchange(
    mut stdin: impl AsyncWrite + Unpin,
    stdout: impl AsyncRead + Unpin,
    mut requests: mpsc::Receiver<WriteItem>,
    events: mpsc::Sender<AvatarEvent>,
    pending: &mut InFlight,
) -> Result<(), AvatarError> {
    let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_RENDERER_LINE));
    loop {
        tokio::select! {
            line = lines.next() => {
                let line = line.ok_or(AvatarError::Exited)?
                    .map_err(|error| AvatarError::OutputFailed(error.to_string()))?;
                let message = serde_json::from_str(&line)
                    .map_err(|error| AvatarError::InvalidJson(error.to_string()))?;
                deliver(message, pending, &events)?;
            }
            request = requests.recv(), if pending.is_none() => {
                let line = match request {
                    Some(WriteItem::Request { id, line, reply }) => {
                        *pending = Some((id, reply));
                        line
                    }
                    Some(WriteItem::OneWay(line)) => line,
                    None => return Ok(()),
                };
                stdin.write_all(line.as_bytes()).await
                    .map_err(|error| AvatarError::WriteFailed(error.to_string()))?;
            }
        }
    }
}

fn deliver(
    message: AvatarMessage,
    pending: &mut InFlight,
    events: &mpsc::Sender<AvatarEvent>,
) -> Result<(), AvatarError> {
    let (id, result) = match message {
        AvatarMessage::Reply { id, .. } => (id, Ok(())),
        AvatarMessage::ErrorReply { id, error } => (id, Err(AvatarError::Renderer(error))),
        AvatarMessage::Fatal(RendererFailure::Error { error }) => {
            return Err(AvatarError::Renderer(error));
        }
        AvatarMessage::Event(event) => {
            return events
                .try_send(event)
                .map_err(|error| AvatarError::OutputFailed(error.to_string()));
        }
    };
    if pending.as_ref().is_some_and(|(waiting, _)| *waiting == id)
        && let Some((_, reply)) = pending.take()
    {
        let _replied = reply.send(result);
    }
    Ok(())
}
