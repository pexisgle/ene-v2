//! The avatar renderer as a child process.
//!
//! `apps/ene-avatar` speaks JSON Lines on stdin and stdout (`src/control.rs`).
//! This module owns that process: it frames requests, drains replies and
//! notifications, and turns a `body.motion` operation into an `add` or a `set`.
//!
//! `set` replaces the whole character configuration, so this module keeps the
//! configuration it last sent and resends it with the one field that changed.
//! Sending only a motion would silently reset the monitor, position and size.
//!
//! The renderer has no network and no host connection. It knows a character id,
//! a model path and a motion path; everything else is this client's job.

use ene_avatar_ipc::{
    AvatarEvent, AvatarMessage, AvatarStatus, CharacterConfig, Command, RendererSnapshot, Request,
};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

pub const AVATAR_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_EVENT_QUEUE: usize = 256;

fn lip_sync_line(character: &str, rms: f32) -> Result<String, String> {
    Request {
        id: None,
        command: Command::LipSync {
            character: character.to_owned(),
            rms: rms.clamp(0., 1.),
        },
    }
    .encode_line()
}

enum WriteItem {
    Request { id: u64, line: String },
    OneWay(String),
}

type PendingReplies = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

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
    pub fn spawn(program: &Path) -> Result<Self, String> {
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
        let mut child = command
            .spawn()
            .map_err(|error| format!("{}: {error}", program.display()))?;
        let stdin = child.stdin.take().ok_or("the renderer has no stdin")?;
        let stdout = child.stdout.take().ok_or("the renderer has no stdout")?;
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
    pub async fn call(&mut self, command: Command) -> Result<Value, String> {
        if !self.healthy {
            return Err("the renderer is no longer healthy".into());
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = Request {
            id: Some(id),
            command,
        }
        .encode_line()?;
        let Some(write_tx) = self.write_tx.clone() else {
            return Err("the renderer input closed".into());
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
            return Err("the renderer input closed".into());
        }
        match tokio::time::timeout(AVATAR_CALL_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.healthy = false;
                Err("the renderer exited".into())
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                self.healthy = false;
                let _ = self.child.kill();
                Err("the renderer timed out answering an IPC command".into())
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
    pub async fn remove_character(&mut self, character: &str) -> Result<(), String> {
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
    ) -> Result<bool, String> {
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
    ) -> Result<bool, String> {
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
    ) -> Result<bool, String> {
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

    pub async fn return_to_idle(&mut self, character: &str, model: &Path) -> Result<(), String> {
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
    ) -> Result<(), String> {
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
    pub async fn expression(&mut self, character: &str, name: Option<&str>) -> Result<(), String> {
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
    ) -> Result<(), String> {
        let clamped = size_percent.clamp(50., 200.);
        let next = match self.shown.get(character) {
            Some(current) => current.with_scale(clamped),
            None => CharacterConfig::for_model(model.to_string_lossy()).with_scale(clamped),
        };
        self.configure(character, next).await
    }

    /// Centers the character on the primary monitor.
    pub async fn center(&mut self, character: &str) -> Result<(), String> {
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
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
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
    pub fn lip_sync(&mut self, character: &str, rms: f32) -> Result<(), String> {
        if !self.healthy {
            return Err("the renderer is no longer healthy".into());
        }
        let Some(write_tx) = &self.write_tx else {
            return Err("the renderer input closed".into());
        };
        let line = lip_sync_line(character, rms)?;
        match write_tx.try_send(WriteItem::OneWay(line)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.healthy = false;
                Err("the renderer input closed".into())
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
    pub async fn await_state(&mut self, character: &str, wait: Duration) -> Result<(), String> {
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
                            let reason = error
                                .clone()
                                .unwrap_or_else(|| "the renderer did not load the model".into());
                            self.push_event(event);
                            return Err(reason);
                        }
                        AvatarStatus::Loading => {}
                    },
                    _ => self.push_event(event),
                },
                Ok(None) | Err(_) => return Ok(()),
            }
        }
    }

    fn take_terminal_state(&mut self, character: &str) -> Option<Result<(), String>> {
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
                .unwrap_or_else(|| "the renderer did not load the model".into()))),
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
            let reason = format!("the renderer input closed: {error}");
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

fn read_loop(
    stdout: impl std::io::Read,
    events_tx: mpsc::UnboundedSender<AvatarEvent>,
    pending: PendingReplies,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut fail_reason = "the renderer exited".to_owned();
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
                            Some(error) => Err(error),
                            None => Ok(ok),
                        });
                    }
                }
                Ok(AvatarMessage::Event(event)) => {
                    let _ = events_tx.send(event);
                }
                Err(error) => {
                    fail_reason = format!("the renderer sent invalid JSON: {error}");
                    break;
                }
            },
            Err(error) => {
                fail_reason = format!("the renderer output failed: {error}");
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

/// Where the development assets live, matching `scripts/fetch-assets.py`.
pub fn asset_cache() -> Option<PathBuf> {
    // The script writes `%LOCALAPPDATA%\ene\cache` on Windows and
    // `$XDG_CACHE_HOME/ene` (no extra level) elsewhere.
    #[cfg(windows)]
    {
        let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        Some(root.join("ene").join("cache"))
    }
    #[cfg(not(windows))]
    {
        let root = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
        Some(root.join("ene"))
    }
}

/// Normalizes `.` and `..` segments onto an absolute path without `\\?\` verbatim
/// prefixes so `ene-avatar` (`AssetPath::try_parse`) accepts the path reliably.
fn normalize_path(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let mut normalized = PathBuf::new();
    for component in abs.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Prefix(prefix) => {
                if let std::path::Prefix::VerbatimDisk(drive) = prefix.kind() {
                    normalized.push(format!("{}:", char::from(drive)));
                } else {
                    normalized.push(prefix.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

/// Adds user data directory candidates for `ENE.vrm` without pulling `shell32.dll`
/// into Windows test binaries via `directories::ProjectDirs`.
fn push_user_model_candidates(candidates: &mut Vec<PathBuf>) {
    #[cfg(windows)]
    {
        for var in ["APPDATA", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var) {
                let base = PathBuf::from(root).join("ene");
                candidates.push(base.join("data").join("models").join("ENE.vrm"));
                candidates.push(base.join("models").join("ENE.vrm"));
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(dirs) = directories::ProjectDirs::from("", "", "ene") {
            candidates.push(dirs.data_dir().join("models").join("ENE.vrm"));
        }
    }
}

/// Searches standard development and installed layouts for the bundled `ENE.vrm`.
pub fn bundled_ene_model() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("assets").join("ENE.vrm"));
        candidates.push(
            dir.join("..")
                .join("share")
                .join("ene")
                .join("assets")
                .join("ENE.vrm"),
        );
        candidates.push(
            dir.join("..")
                .join("Resources")
                .join("assets")
                .join("ENE.vrm"),
        );
        candidates.push(dir.join("..").join("assets").join("ENE.vrm"));
        candidates.push(dir.join("..").join("..").join("assets").join("ENE.vrm"));
    }
    candidates.push(PathBuf::from("assets").join("ENE.vrm"));
    push_user_model_candidates(&mut candidates);
    if let Some(cache) = asset_cache() {
        candidates.push(cache.join("models").join("ENE.vrm"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("ENE.vrm"),
    );

    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| normalize_path(&path))
}

/// Resolves a `Companion.body` model reference (`"ENE.vrm"`, `"Seed-san.vrm"`, or an
/// existing `.vrm` path without parent traversal) with fallback to the default model.
pub fn resolve_model_path(spec: Option<&str>, fallback: Option<&Path>) -> Option<PathBuf> {
    if let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) {
        let candidate_path = Path::new(raw);
        let has_parent = candidate_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
        if !has_parent && raw.to_ascii_lowercase().ends_with(".vrm") {
            if candidate_path.is_absolute() && candidate_path.is_file() {
                if let Some(clean) = normalize_path(candidate_path) {
                    return Some(clean);
                }
            } else if candidate_path.components().count() == 1 {
                let file_name = candidate_path.file_name()?;
                if file_name.eq_ignore_ascii_case("ENE.vrm")
                    && let Some(found) = bundled_ene_model()
                {
                    return Some(found);
                }
                let mut dirs = Vec::new();
                if let Some(bundled) =
                    bundled_ene_model().and_then(|p| p.parent().map(Path::to_path_buf))
                {
                    dirs.push(bundled);
                }
                if let Some(cache) = asset_cache() {
                    dirs.push(cache.join("models"));
                }
                #[cfg(windows)]
                for var in ["APPDATA", "LOCALAPPDATA"] {
                    if let Some(root) = std::env::var_os(var) {
                        let base = PathBuf::from(root).join("ene");
                        dirs.push(base.join("data").join("models"));
                        dirs.push(base.join("models"));
                    }
                }
                for dir in dirs {
                    let full = dir.join(file_name);
                    if full.is_file()
                        && let Some(clean) = normalize_path(&full)
                    {
                        return Some(clean);
                    }
                }
            }
        }
    }
    fallback.map(Path::to_path_buf)
}

/// The default model: the bundled `ENE.vrm` first, then the fetched sample cache.
pub fn default_model() -> Option<PathBuf> {
    if let Some(path) = bundled_ene_model() {
        return Some(path);
    }
    let path = asset_cache()?.join("models").join("Seed-san.vrm");
    path.is_file().then_some(path)
}

/// Searches adjacent and Cargo build directories for the `ene-avatar` renderer binary.
pub fn default_renderer() -> Option<PathBuf> {
    let binary = format!("ene-avatar{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        dir.join(&binary),
        dir.join("..").join("release").join(&binary),
        dir.join("..").join("debug").join(&binary),
    ];
    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| normalize_path(&path))
}

pub const MOTIONS: &[(&str, &str)] = &[
    // Existing 11 core motions
    ("idle", "idle-breath.vrma"),
    ("look_around", "idle-look-around.vrma"),
    ("adjust_posture", "guarded-idle.vrma"),
    ("sleepy_nod", "sleepy-nod.vrma"),
    ("nod", "acknowledge.vrma"),
    ("tilt_head", "task-think.vrma"),
    ("stretch", "break-stretch.vrma"),
    ("wave", "welcome-back.vrma"),
    ("ask_question", "ask-question.vrma"),
    ("task_complete", "task-complete.vrma"),
    ("apologize", "apologize.vrma"),
    // Expanded Kimodo DesktopMate & companion motions (24)
    ("idle_sway", "idle-sway.vrma"),
    ("idle_hands_behind", "idle-hands-behind.vrma"),
    ("yawn", "yawn.vrma"),
    ("deep_breath", "deep-breath.vrma"),
    ("shrug", "shrug.vrma"),
    ("bow", "polite-bow.vrma"),
    ("goodbye", "goodbye-wave.vrma"),
    ("peek", "peek-curious.vrma"),
    ("beckon", "beckon.vrma"),
    ("head_pat", "head-pat.vrma"),
    ("surprised", "tap-surprise.vrma"),
    ("shy", "shy-fidget.vrma"),
    ("happy_bounce", "happy-bounce.vrma"),
    ("clap", "clap.vrma"),
    ("cheer", "cheer-encourage.vrma"),
    ("music_groove", "music-groove.vrma"),
    ("sit_idle", "perch-idle.vrma"),
    ("sit_swing_legs", "perch-kick.vrma"),
    ("sit_wave", "perch-greet.vrma"),
    ("drag_held", "drag-held.vrma"),
    ("listen", "listen-active.vrma"),
    ("explain", "speak-explain.vrma"),
    ("shake_head", "disagree.vrma"),
    ("aha", "aha.vrma"),
];

pub const LOOPING_MOTIONS: &[&str] = &[
    "idle",
    "idle_sway",
    "idle_hands_behind",
    "music_groove",
    "sit_idle",
    "sit_swing_legs",
    "drag_held",
];

pub fn is_looping_motion(name: &str) -> bool {
    LOOPING_MOTIONS.contains(&name)
}

fn is_persistent_idle(name: &str) -> bool {
    matches!(
        name,
        "idle" | "idle_sway" | "idle_hands_behind" | "sit_idle" | "sit_swing_legs"
    )
}

fn is_seated_motion(name: &str) -> bool {
    matches!(name, "sit_idle" | "sit_swing_legs" | "sit_wave")
}

fn idle_return_preset(base: Option<&str>, finished: Option<&str>) -> &'static str {
    if finished.is_some_and(is_seated_motion) {
        return match base {
            Some("sit_swing_legs") => "sit_swing_legs",
            _ => "sit_idle",
        };
    }
    match base {
        Some("idle_sway") => "idle_sway",
        Some("idle_hands_behind") => "idle_hands_behind",
        _ => "idle",
    }
}

/// The selected VRMAs ship beside the bundled ENE model.
pub fn motion(name: &str) -> Option<PathBuf> {
    let (_, file) = MOTIONS.iter().find(|(preset, _)| *preset == name)?;
    let model = bundled_ene_model()?;
    let path = model.parent()?.join("vrma").join(file);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_and_a_notification_are_distinguished() {
        let reply: AvatarMessage = serde_json::from_str(r#"{"id":1,"ok":null}"#).unwrap();
        assert!(matches!(reply, AvatarMessage::Reply { id: 1, .. }));
        let failed: AvatarMessage =
            serde_json::from_str(r#"{"id":2,"error":"unknown character"}"#).unwrap();
        assert!(matches!(
            failed,
            AvatarMessage::Reply { error: Some(_), .. }
        ));
        let event: AvatarMessage =
            serde_json::from_str(r#"{"event":"state","character":"ene","status":"ready"}"#)
                .unwrap();
        assert!(matches!(event, AvatarMessage::Event(_)));
    }

    #[test]
    fn bundled_presets_resolve_to_vrma_files() {
        use std::collections::BTreeSet;

        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/vrma");
        let shipped: BTreeSet<_> = assets
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let selected: BTreeSet<std::ffi::OsString> =
            MOTIONS.iter().map(|&(_, file)| file.into()).collect();
        assert_eq!(shipped, selected);

        for &(preset, _) in MOTIONS {
            let path = motion(preset).expect(preset);
            assert!(path.is_absolute() && path.is_file(), "{path:?}");
        }
        assert!(motion("no-such-motion").is_none());
    }

    #[test]
    fn looping_postures_and_idle_return_preserve_resting_state() {
        assert!(is_looping_motion("idle"));
        assert!(is_looping_motion("idle_sway"));
        assert!(is_looping_motion("idle_hands_behind"));
        assert!(is_looping_motion("sit_idle"));
        assert!(is_looping_motion("sit_swing_legs"));
        assert!(is_looping_motion("music_groove"));
        assert!(is_looping_motion("drag_held"));
        assert!(!is_looping_motion("nod"));
        assert!(!is_looping_motion("sit_wave"));

        for &preset in LOOPING_MOTIONS {
            assert!(
                MOTIONS.iter().any(|&(name, _)| name == preset),
                "looping preset {preset} must be declared in MOTIONS"
            );
        }

        assert_eq!(idle_return_preset(None, Some("nod")), "idle");
        assert_eq!(
            idle_return_preset(Some("idle_sway"), Some("nod")),
            "idle_sway"
        );
        assert_eq!(
            idle_return_preset(Some("idle_hands_behind"), Some("clap")),
            "idle_hands_behind"
        );
        assert_eq!(
            idle_return_preset(Some("sit_idle"), Some("sit_wave")),
            "sit_idle"
        );
        assert_eq!(
            idle_return_preset(Some("sit_swing_legs"), Some("sit_wave")),
            "sit_swing_legs"
        );
        // A standing gesture played from a seated base returns to standing idle.
        assert_eq!(idle_return_preset(Some("sit_idle"), Some("wave")), "idle");
    }

    /// The renderer's `set` replaces the whole configuration, so changing the
    /// motion must carry the placement that was already there.
    #[test]
    fn changing_a_motion_keeps_the_placement_fields() {
        let mut shown = CharacterConfig::for_model("/models/ene.vrm");
        shown.monitor = Some("eDP-1".into());
        shown.position = Some([100., 200.]);
        shown.size_percent = 140.;
        let mut next = shown.clone();
        next.motion = Some("/motions/nod.vrma".into());
        let config = serde_json::to_value(&next).unwrap();
        assert_eq!(config["monitor"], "eDP-1");
        assert_eq!(config["position"][1], 200.0);
        assert_eq!(config["size_percent"], 140.0);
        assert_eq!(config["motion"], "/motions/nod.vrma");
    }

    #[test]
    fn a_face_request_can_be_set_and_cleared() {
        let set = serde_json::to_value(Command::Expression {
            character: "ene".into(),
            name: Some("happy".into()),
        })
        .unwrap();
        assert_eq!(set["character"], "ene");
        assert_eq!(set["name"], "happy");
        let clear = serde_json::to_value(Command::Expression {
            character: "ene".into(),
            name: None,
        })
        .unwrap();
        assert!(clear["name"].is_null());
    }

    #[test]
    fn the_lip_line_is_one_way_and_clamped() {
        let line = lip_sync_line("ene", 2.5).unwrap();
        assert!(line.ends_with('\n'));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["op"], "lip_sync");
        assert_eq!(parsed["character"], "ene");
        assert_eq!(parsed["rms"], 1.0);
        assert!(parsed.get("id").is_none());

        let parsed: Value =
            serde_json::from_str(lip_sync_line("ene", -1.0).unwrap().trim_end()).unwrap();
        assert_eq!(parsed["rms"], 0.0);
    }

    #[test]
    fn scaling_keeps_motion_and_placement_fields() {
        let mut shown = CharacterConfig::for_model("/models/ene.vrm");
        shown.motion = Some("/motions/nod.vrma".into());
        shown.monitor = Some("eDP-1".into());
        shown.position = Some([120., 240.]);
        let scaled = shown.with_scale(250.0);
        assert_eq!(scaled.size_percent, 200.0);
        assert_eq!(scaled.motion.as_deref(), Some("/motions/nod.vrma"));
        assert_eq!(scaled.monitor.as_deref(), Some("eDP-1"));
        assert_eq!(scaled.position, Some([120., 240.]));

        let min_scaled = shown.with_scale(10.0);
        assert_eq!(min_scaled.size_percent, 50.0);
    }

    #[test]
    fn b09_read_loop_resolves_pending_and_preserves_all_event_kinds() {
        let input = concat!(
            r#"{"event":"click","character":"ene"}"#,
            "\n",
            r#"{"event":"state","character":"ene","status":"ready"}"#,
            "\n",
            r#"{"event":"finished","character":"ene"}"#,
            "\n",
            r#"{"id":7,"ok":{"status":"done"}}"#,
            "\n"
        );
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let pending: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
        let (reply_tx, mut reply_rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, reply_tx);
        let (waiter_tx, mut waiter_rx) = oneshot::channel();
        pending.lock().unwrap().insert(8, waiter_tx);

        read_loop(input.as_bytes(), events_tx, Arc::clone(&pending));

        let mut events = Vec::new();
        while let Ok(ev) = events_rx.try_recv() {
            events.push(ev);
        }
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], AvatarEvent::Click { .. }));
        assert!(matches!(events[1], AvatarEvent::State { .. }));
        assert!(matches!(events[2], AvatarEvent::Finished { .. }));
        assert_eq!(
            reply_rx.try_recv().unwrap().unwrap(),
            serde_json::json!({"status":"done"})
        );
        assert!(
            waiter_rx.try_recv().unwrap().is_err(),
            "EOF must wake unfinished pending waiters"
        );
    }

    #[test]
    fn normalize_path_cleans_parent_segments_without_verbatim_prefix() {
        let normalized = normalize_path(Path::new("assets/../assets/ENE.vrm")).unwrap();
        let text = normalized.to_string_lossy();
        assert!(
            !text.starts_with(r"\\?\"),
            "unexpected verbatim prefix: {text}"
        );
        assert!(text.ends_with("ENE.vrm"), "unexpected suffix: {text}");
        assert!(
            !normalized.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )),
            "unnormalized components remained: {normalized:?}"
        );
    }

    #[test]
    fn bundled_ene_model_resolves_workspace_asset() {
        let workspace_asset = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("ENE.vrm");
        if workspace_asset.is_file() {
            let found = bundled_ene_model().expect("bundled ENE.vrm resolves");
            assert!(found.is_file(), "resolved path does not exist: {found:?}");
            assert!(
                found.is_absolute(),
                "resolved path is not absolute: {found:?}"
            );
            assert_eq!(found.file_name().and_then(|n| n.to_str()), Some("ENE.vrm"));
        }
    }
}
