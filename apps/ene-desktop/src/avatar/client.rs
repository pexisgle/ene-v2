use super::presets::{idle_return_preset, is_looping_motion, is_persistent_idle, motion};
use ene_avatar_ipc::{
    AvatarEvent, AvatarMessage, AvatarStatus, CharacterConfig, Command, RendererSnapshot, Request,
};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Stdio},
    sync::{Arc, Mutex},
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

enum WriteItem {
    Request { id: u64, line: String },
    OneWay(String),
}

pub(super) type PendingReplies =
    Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, AvatarError>>>>>;

/// The renderer process and non-blocking async channels to its stdio workers.
pub struct Avatar {
    child: Child,
    write_tx: Option<mpsc::Sender<WriteItem>>,
    events_rx: mpsc::UnboundedReceiver<AvatarEvent>,
    pending: PendingReplies,
    events: VecDeque<AvatarEvent>,
    next_id: u64,
    healthy: bool,
    shown: HashMap<String, CharacterConfig>,
    base_motion: HashMap<String, String>,
    last_motion: HashMap<String, String>,
}

impl Avatar {
    #[cfg(windows)]
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
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
        let writer_pending = Arc::clone(&pending);
        thread::spawn(move || write_loop(stdin, write_rx, writer_pending));
        let reader_pending = Arc::clone(&pending);
        thread::spawn(move || read_loop(stdout, events_tx, reader_pending));
        Ok(Self {
            child,
            write_tx: Some(write_tx),
            events_rx,
            pending,
            events: VecDeque::new(),
            next_id: 1,
            healthy: true,
            shown: HashMap::new(),
            base_motion: HashMap::new(),
            last_motion: HashMap::new(),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    fn push_event(&mut self, event: AvatarEvent) {
        if self.events.len() >= MAX_EVENT_QUEUE {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn drain_incoming_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.push_event(event);
        }
    }

    /// Sends one command and awaits its reply with a bounded timeout so a stalled
    /// renderer never freezes the Tokio thread.
    pub async fn call(&mut self, command: Command) -> Result<Value, AvatarError> {
        if !self.healthy {
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
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, reply_tx);
        if write_tx
            .send(WriteItem::Request { id, line })
            .await
            .is_err()
        {
            self.pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&id);
            self.healthy = false;
            return Err(AvatarError::InputClosed);
        }
        match tokio::time::timeout(AVATAR_CALL_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.healthy = false;
                Err(AvatarError::Exited)
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                self.healthy = false;
                let _ = self.child.kill();
                Err(AvatarError::Timeout)
            }
        }
    }

    /// Drains all buffered notifications without dropping click or state events.
    pub fn notifications(&mut self) -> Vec<AvatarEvent> {
        self.drain_incoming_events();
        self.events.drain(..).collect()
    }

    pub fn shown_characters(&self) -> Vec<String> {
        self.shown.keys().cloned().collect()
    }

    /// Removes `character` from the renderer when its companion is released.
    pub async fn remove_character(&mut self, character: &str) -> Result<(), AvatarError> {
        self.base_motion.remove(character);
        self.last_motion.remove(character);
        if self.shown.remove(character).is_some() {
            self.call(Command::Remove {
                character: character.to_owned(),
            })
            .await?;
        }
        Ok(())
    }

    /// Shows `character` applying a `Companion.body` configuration (`model`, `monitor`,
    /// `position`, `size_percent`).
    pub async fn show_with_body(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
        body: &ene_rpc::BodyBinding,
        default_scale: f32,
    ) -> Result<bool, AvatarError> {
        let existing = self.shown.get(character).cloned();
        let mut next = existing
            .clone()
            .unwrap_or_else(|| CharacterConfig::for_model(model.to_string_lossy()));
        next.model = model.to_string_lossy().into_owned();
        if existing.is_none() {
            next.motion = motion.map(|p| p.to_string_lossy().into_owned());
            next.looping = true;
        }
        if body.monitor.is_some() {
            next.monitor = body.monitor.clone();
        }
        if body.position.is_some() {
            next.position = body.position;
        }
        next.size_percent = body.size_percent.unwrap_or(default_scale).clamp(50., 200.);
        if existing.as_ref() == Some(&next) {
            return Ok(false);
        }
        let loads_model = existing
            .as_ref()
            .is_none_or(|shown| shown.model != next.model);
        self.configure(character, next).await?;
        Ok(loads_model)
    }

    /// Shows `motion` on `character`, adding it first if it is not there.
    pub async fn show(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
        looping: bool,
    ) -> Result<bool, AvatarError> {
        let motion = motion.map(|path| path.to_string_lossy().into_owned());
        let existing = self.shown.get(character);
        let mut next = existing
            .cloned()
            .unwrap_or_else(|| CharacterConfig::for_model(model.to_string_lossy()));
        next.model = model.to_string_lossy().into_owned();
        next.motion = motion;
        next.looping = looping;
        if looping && existing == Some(&next) {
            return Ok(false);
        }
        let loads_model = existing.is_none_or(|shown| shown.model != next.model);
        if loads_model {
            self.configure(character, next).await?;
        } else {
            self.call(Command::Motion {
                character: character.to_owned(),
                motion: next.motion.clone(),
                looping,
            })
            .await?;
            self.shown.insert(character.to_owned(), next);
        }
        self.drain_incoming_events();
        self.events.retain(|event| {
            !(matches!(event, AvatarEvent::Finished { .. }) && event.character() == character)
        });
        Ok(loads_model)
    }

    /// Shows a declared motion preset, tracking looping resting postures for return-to-idle.
    pub async fn show_preset(
        &mut self,
        character: &str,
        model: &Path,
        preset: &str,
        clip: Option<&Path>,
    ) -> Result<bool, AvatarError> {
        let looping = is_looping_motion(preset);
        let loads_model = self.show(character, model, clip, looping).await?;
        self.last_motion
            .insert(character.to_owned(), preset.to_owned());
        if is_persistent_idle(preset) {
            self.base_motion
                .insert(character.to_owned(), preset.to_owned());
        }
        Ok(loads_model)
    }

    pub async fn return_to_idle(
        &mut self,
        character: &str,
        model: &Path,
    ) -> Result<(), AvatarError> {
        if self
            .shown
            .get(character)
            .is_some_and(|shown| !shown.looping)
        {
            let preset = idle_return_preset(
                self.base_motion.get(character).map(String::as_str),
                self.last_motion.get(character).map(String::as_str),
            );
            self.show_preset(character, model, preset, motion(preset).as_deref())
                .await?;
        }
        Ok(())
    }

    async fn configure(
        &mut self,
        character: &str,
        mut next: CharacterConfig,
    ) -> Result<(), AvatarError> {
        let existing = self.shown.contains_key(character);
        if existing {
            let state = self.call(Command::Get {}).await?;
            if let Ok(snapshot) = serde_json::from_value::<RendererSnapshot>(state)
                && let Some(actor) = snapshot
                    .characters
                    .into_iter()
                    .find(|actor| actor.character == character)
            {
                next.monitor = actor.config.monitor;
                next.position = actor.config.position;
            }
        }
        let command = if existing {
            Command::Set {
                character: character.to_owned(),
                config: next.clone(),
            }
        } else {
            Command::Add {
                character: character.to_owned(),
                config: next.clone(),
            }
        };
        self.call(command).await?;
        self.shown.insert(character.to_owned(), next);
        Ok(())
    }

    /// Sets the character's face, or clears it with `None`.
    pub async fn expression(
        &mut self,
        character: &str,
        name: Option<&str>,
    ) -> Result<(), AvatarError> {
        self.call(Command::Expression {
            character: character.to_owned(),
            name: name.map(str::to_owned),
        })
        .await?;
        Ok(())
    }

    /// Updates the character's display scale while keeping motion and placement.
    pub async fn set_scale(
        &mut self,
        character: &str,
        model: &Path,
        size_percent: f32,
    ) -> Result<(), AvatarError> {
        let clamped = size_percent.clamp(50., 200.);
        let next = match self.shown.get(character) {
            Some(current) => current.with_scale(clamped),
            None => CharacterConfig::for_model(model.to_string_lossy()).with_scale(clamped),
        };
        self.configure(character, next).await
    }

    /// Centers the character on the primary monitor.
    pub async fn center(&mut self, character: &str) -> Result<(), AvatarError> {
        self.call(Command::Center {
            character: character.to_owned(),
            monitor: None,
        })
        .await?;
        Ok(())
    }

    /// Sets or clears direct gaze angles in degrees (`None` returns gaze to animation/neutral).
    pub async fn look_at(
        &mut self,
        character: &str,
        yaw_deg: Option<f32>,
        pitch_deg: Option<f32>,
    ) -> Result<(), AvatarError> {
        self.call(Command::LookAt {
            character: character.to_owned(),
            yaw_deg,
            pitch_deg,
        })
        .await?;
        Ok(())
    }

    /// Places the character's foot position on an optional monitor (logical pixels).
    pub async fn place(
        &mut self,
        character: &str,
        monitor: Option<String>,
        position: [f32; 2],
    ) -> Result<(), AvatarError> {
        self.call(Command::Place {
            character: character.to_owned(),
            monitor: monitor.clone(),
            position,
        })
        .await?;
        if let Some(shown) = self.shown.get_mut(character) {
            if monitor.is_some() {
                shown.monitor = monitor;
            }
            shown.position = Some(position);
        }
        Ok(())
    }

    /// Queues one mouth amplitude non-blockingly; drops frames when congested.
    pub fn lip_sync(&mut self, character: &str, rms: f32) -> Result<(), AvatarError> {
        if !self.healthy {
            return Err(AvatarError::Unhealthy);
        }
        let Some(write_tx) = &self.write_tx else {
            return Err(AvatarError::InputClosed);
        };
        let line = lip_sync_line(character, rms)?;
        match write_tx.try_send(WriteItem::OneWay(line)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.healthy = false;
                Err(AvatarError::InputClosed)
            }
        }
    }

    /// What the renderer is showing, for the device footer.
    pub fn describe(&self, character: &str) -> Option<String> {
        self.shown.get(character).map(|shown| {
            let model = shown
                .model
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&shown.model)
                .to_owned();
            match shown
                .motion
                .as_ref()
                .and_then(|motion| motion.rsplit(['\\', '/']).next())
            {
                Some(motion) => format!("{model} ({motion})"),
                None => model,
            }
        })
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
                                .map(AvatarError::Renderer)
                                .unwrap_or(AvatarError::ModelLoadFailed);
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
        match &self.events[pos] {
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
                .map(AvatarError::Renderer)
                .unwrap_or(AvatarError::ModelLoadFailed))),
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
            let _ = write_tx.try_send(WriteItem::OneWay(line));
        }
        let deadline = std::time::Instant::now() + AVATAR_SHUTDOWN_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(15));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

impl Drop for Avatar {
    fn drop(&mut self) {
        self.write_tx.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::Receiver<WriteItem>, pending: PendingReplies) {
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
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id)
            {
                let _ = tx.send(Err(reason));
            }
            break;
        }
    }
}

pub(super) fn read_loop(
    stdout: impl std::io::Read,
    events_tx: mpsc::UnboundedSender<AvatarEvent>,
    pending: PendingReplies,
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
                        .unwrap_or_else(|poison| poison.into_inner())
                        .remove(&id)
                    {
                        let _ = tx.send(match error {
                            Some(error) => Err(AvatarError::Renderer(error)),
                            None => Ok(ok),
                        });
                    }
                }
                Ok(AvatarMessage::Event(event)) => {
                    let _ = events_tx.send(event);
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
        .unwrap_or_else(|poison| poison.into_inner())
        .drain()
        .map(|(_, tx)| tx)
        .collect();
    for tx in waiters {
        let _ = tx.send(Err(fail_reason.clone()));
    }
}
