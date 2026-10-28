//! Renderer child-process lifecycle over the JSON Lines control protocol.
//!
//! The renderer owns VRM loading, window placement and pointer input. This
//! module keeps one character identity alive, forwards preference changes and
//! turns renderer events into bounded notifications for the UI.

mod contracts;

pub use contracts::{
    AvatarConfig, AvatarEvent, AvatarId, AvatarPlacement, AvatarState, MotionState,
};

use super::settings::AvatarPreferences;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fmt, io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, watch},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const CAPACITY: usize = 64;
/// The renderer flushes queued output before exiting, so give it a moment.
const REAP: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Inbox {
    state: Option<AvatarState>,
    state_changed: bool,
    events: VecDeque<AvatarEvent>,
    error: Option<String>,
    notified: bool,
}

#[derive(Clone)]
struct Sink {
    id: Arc<Mutex<Option<AvatarId>>>,
    next_id: Arc<AtomicU64>,
    inbox: Arc<Mutex<Inbox>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Sink {
    /// Show a character under an identity that was never used in this session.
    fn show(&self) -> AvatarId {
        self.reset();
        let id = AvatarId(self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        *self.id.lock().unwrap() = Some(id);
        id
    }
    fn reset(&self) {
        *self.id.lock().unwrap() = None;
        *self.inbox.lock().unwrap() = Inbox::default();
    }
    fn event(&self, event: AvatarEvent) {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.events.len() == CAPACITY {
            inbox.error = Some("描画イベントのキューが満杯です。".into());
        } else {
            inbox.events.push_back(event);
        }
        self.notify(&mut inbox);
    }
    fn state(&self, state: AvatarState) {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.state.as_ref() != Some(&state) {
            inbox.state = Some(state);
            inbox.state_changed = true;
            self.notify(&mut inbox);
        }
    }
    fn error(&self, message: impl fmt::Display) {
        let mut inbox = self.inbox.lock().unwrap();
        inbox.error = Some(message.to_string());
        self.notify(&mut inbox);
    }
    fn notify(&self, inbox: &mut Inbox) {
        if !inbox.notified {
            inbox.notified = true;
            (self.wake)();
        }
    }
}

pub struct Notifications {
    pub state: Option<AvatarState>,
    pub events: Vec<AvatarEvent>,
    pub error: Option<String>,
}

enum Control {
    Recenter,
    Restart,
}

pub struct Controller {
    commands: mpsc::Sender<Control>,
    config: watch::Sender<Option<AvatarPreferences>>,
    placement: watch::Sender<AvatarPlacement>,
    stop: CancellationToken,
    sink: Sink,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Controller {
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let executable = std::env::var_os("ENE_AVATAR_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| ene_platform::sibling_executable("ene-avatar"));
        Self::with_executable(executable, wake)
    }
    fn with_executable(executable: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (commands, controls) = mpsc::channel(CAPACITY);
        let (config, updates) = watch::channel(None);
        let (placement, placements) = watch::channel(AvatarPlacement::default());
        let stop = CancellationToken::new();
        let sink = Sink {
            id: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(0)),
            inbox: Arc::new(Mutex::new(Inbox::default())),
            wake,
        };
        let worker_sink = sink.clone();
        let worker_stop = stop.clone();
        let worker_placement = placement.clone();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("avatar runtime");
            runtime.block_on(supervise(
                executable,
                controls,
                updates,
                worker_placement,
                placements,
                worker_stop,
                worker_sink,
            ));
        });
        Self {
            commands,
            config,
            placement,
            stop,
            sink,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn configure(&self, config: AvatarPreferences) {
        self.config.send_if_modified(|current| {
            if current.as_ref() == Some(&config) {
                false
            } else {
                *current = Some(config);
                true
            }
        });
    }
    pub fn placement(&self, placement: AvatarPlacement) {
        self.placement.send_if_modified(|current| {
            if *current == placement {
                false
            } else {
                *current = placement;
                true
            }
        });
    }
    fn command(&self, command: Control) {
        if self.commands.try_send(command).is_err() {
            self.sink
                .error("描画操作のキューが満杯、または通信が切断されました。");
            self.stop.cancel();
        }
    }
    pub fn restart(&self) {
        self.command(Control::Restart);
    }
    pub fn recenter(&self) {
        self.command(Control::Recenter);
    }
    pub fn drain(&self) -> Notifications {
        let mut inbox = self.sink.inbox.lock().unwrap();
        let state = inbox.state_changed.then(|| inbox.state.clone()).flatten();
        inbox.state_changed = false;
        let count = inbox.events.len().min(32);
        let notifications = Notifications {
            state,
            events: inbox.events.drain(..count).collect(),
            error: inbox.error.take(),
        };
        inbox.notified = false;
        if !inbox.events.is_empty() {
            self.sink.notify(&mut inbox);
        }
        notifications
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Writes one request per line and tracks the correlation ID.
struct Client {
    stdin: ChildStdin,
    next: u64,
}

impl Client {
    async fn request(&mut self, mut request: Value) -> io::Result<()> {
        self.next += 1;
        request["id"] = json!(self.next);
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        self.stdin.flush().await
    }
    async fn show(&mut self, id: AvatarId, session: &Session) -> io::Result<()> {
        self.request(json!({
            "op": "add",
            "character": id.to_string(),
            "config": session.renderer_config(),
        }))
        .await
    }
    async fn update(&mut self, id: AvatarId, session: &Session) -> io::Result<()> {
        self.request(json!({
            "op": "set",
            "character": id.to_string(),
            "config": session.renderer_config(),
        }))
        .await
    }
    async fn hide(&mut self, id: AvatarId) -> io::Result<()> {
        self.request(json!({"op": "remove", "character": id.to_string()}))
            .await
    }
    async fn recenter(&mut self, id: AvatarId) -> io::Result<()> {
        self.request(json!({
            "op": "center",
            "character": id.to_string(),
            "monitor": Value::Null,
        }))
        .await
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Loading,
    Ready,
    Suspended,
    Failed,
}

/// One live character as the UI understands it.
struct Session {
    id: AvatarId,
    config: AvatarConfig,
    placement: AvatarPlacement,
    status: Status,
    error: Option<String>,
    finished: bool,
}

impl Session {
    fn new(id: AvatarId, config: AvatarConfig, placement: AvatarPlacement) -> Self {
        Self {
            id,
            config,
            placement,
            status: Status::Loading,
            error: None,
            finished: false,
        }
    }
    /// The renderer takes a complete config on every write, so placement joins it here.
    fn renderer_config(&self) -> Value {
        json!({
            "model": self.config.model,
            "motion": self.config.motion,
            "playing": self.config.motion_playing,
            "looping": self.config.motion_loop,
            "monitor": self.placement.monitor,
            "position": self.placement.position,
            "size_percent": self.config.size_percent,
        })
    }
    fn snapshot(&self) -> AvatarState {
        let motion = match (&self.config.motion, self.status) {
            (None, _) => MotionState::None,
            (_, Status::Failed) => MotionState::Failed {
                message: self.error.clone().unwrap_or_default(),
            },
            (_, Status::Loading) => MotionState::Loading,
            (Some(_), _) if !self.config.motion_playing => MotionState::Paused,
            (Some(_), _) if self.finished => MotionState::Finished,
            (Some(_), _) => MotionState::Playing,
        };
        AvatarState {
            placement: self.placement.clone(),
            loading: self.status == Status::Loading,
            model: (!self.config.model.is_empty()).then(|| self.config.model.clone()),
            motion,
            error: self.error.clone(),
        }
    }
    fn apply(&mut self, line: &str, sink: &Sink, placement: &watch::Sender<AvatarPlacement>) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            sink.error("描画プロセスの応答を読めません。");
            return;
        };
        if value.get("id").is_some() {
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                sink.error(error);
                return;
            }
            // `get` resolves the placement the renderer would use, which is the
            // only way to learn where a recentered character landed.
            if let Some(config) = value
                .pointer("/ok/characters")
                .and_then(Value::as_array)
                .and_then(|characters| {
                    characters
                        .iter()
                        .find(|character| character["character"] == self.id.to_string())
                })
                .map(|character| &character["config"])
                && self.take_placement(config, placement)
            {
                sink.state(self.snapshot());
            }
            return;
        }
        if value.get("character").and_then(Value::as_str) != Some(self.id.to_string().as_str()) {
            return;
        }
        match value.get("event").and_then(Value::as_str) {
            Some("state") => {
                self.status = match value.get("status").and_then(Value::as_str) {
                    Some("ready") => Status::Ready,
                    Some("suspended") => Status::Suspended,
                    Some("error") => Status::Failed,
                    _ => Status::Loading,
                };
                self.error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("finished") => {
                self.finished = true;
                sink.event(AvatarEvent::MotionFinished);
            }
            Some("click") => sink.event(AvatarEvent::ShowChat),
            Some("moved") => {
                self.take_placement(&value, placement);
            }
            _ => {}
        }
        sink.state(self.snapshot());
    }

    /// Adopts the renderer's resolved monitor and position when it reports one.
    fn take_placement(
        &mut self,
        config: &Value,
        placement: &watch::Sender<AvatarPlacement>,
    ) -> bool {
        let Some((x, y)) = config
            .get("position")
            .and_then(Value::as_array)
            .and_then(|position| Some((position.first()?.as_f64()?, position.get(1)?.as_f64()?)))
        else {
            return false;
        };
        self.placement = AvatarPlacement {
            position: Some([x.round() as i32, y.round() as i32]),
            monitor: config
                .get("monitor")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        placement.send_replace(self.placement.clone());
        true
    }
}

async fn supervise(
    executable: PathBuf,
    mut controls: mpsc::Receiver<Control>,
    mut config: watch::Receiver<Option<AvatarPreferences>>,
    placement: watch::Sender<AvatarPlacement>,
    mut placements: watch::Receiver<AvatarPlacement>,
    stop: CancellationToken,
    sink: Sink,
) {
    loop {
        let initial = loop {
            if let Some(initial) = config.borrow_and_update().clone() {
                break initial;
            }
            tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                changed = config.changed() => if changed.is_err() { return },
            }
        };
        if stop.is_cancelled() {
            return;
        }
        sink.reset();
        let mut child = match spawn(&executable) {
            Ok(child) => child,
            Err(error) => {
                sink.error(format!("キャラクターを起動できません: {error}"));
                if !wait_restart(&mut controls, &stop).await {
                    return;
                }
                continue;
            }
        };
        let mut client = Client {
            stdin: child.stdin.take().expect("renderer stdin"),
            next: 0,
        };
        let mut lines: Lines<_> =
            BufReader::new(child.stdout.take().expect("renderer stdout")).lines();
        let mut session = initial.visible.then(|| {
            Session::new(
                sink.show(),
                initial.config.clone(),
                placement.borrow().clone(),
            )
        });
        let mut fps = initial.fps;
        let mut restart_requested = false;
        if let Err(error) = client.request(json!({"op": "fps", "value": fps})).await {
            sink.error(error);
        } else if let Some(session) = &session
            && let Err(error) = client.show(session.id, session).await
        {
            sink.error(error);
        }
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                command = controls.recv() => match command {
                    Some(Control::Restart) => {
                        restart_requested = true;
                        break;
                    }
                    Some(Control::Recenter) => {
                        if let Some(session) = &session {
                            let centered = client.recenter(session.id).await;
                            let resolved = centered.and(client.request(json!({"op": "get"})).await);
                            if let Err(error) = resolved {
                                sink.error(error);
                                break;
                            }
                        }
                    }
                    None => return,
                },
                changed = config.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let Some(next) = config.borrow_and_update().clone() else {
                        break;
                    };
                    if next.visible != session.is_some() {
                        match session.take() {
                            Some(previous) => {
                                sink.reset();
                                if let Err(error) = client.hide(previous.id).await {
                                    sink.error(error);
                                    break;
                                }
                            }
                            None => {
                                let next_session = Session::new(
                                    sink.show(),
                                    next.config.clone(),
                                    placement.borrow().clone(),
                                );
                                if let Err(error) = client.show(next_session.id, &next_session).await {
                                    sink.error(error);
                                    break;
                                }
                                session = Some(next_session);
                            }
                        }
                    } else if let Some(session) = &mut session
                        && session.config != next.config
                    {
                        session.config = next.config.clone();
                        session.finished = false;
                        if let Err(error) = client.update(session.id, session).await {
                            sink.error(error);
                            break;
                        }
                        sink.state(session.snapshot());
                    }
                    if fps != next.fps {
                        fps = next.fps;
                        if let Err(error) = client.request(json!({"op": "fps", "value": fps})).await {
                            sink.error(error);
                            break;
                        }
                    }
                },
                changed = placements.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let Some(session) = &mut session else {
                        placements.borrow_and_update();
                        continue;
                    };
                    let latest = placements.borrow_and_update().clone();
                    if latest == session.placement {
                        continue;
                    }
                    session.placement = latest;
                    if let Err(error) = client.update(session.id, session).await {
                        sink.error(error);
                        break;
                    }
                    sink.state(session.snapshot());
                },
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Some(session) = &mut session {
                            session.apply(&line, &sink, &placement);
                        }
                    }
                    Ok(None) => {
                        sink.error("描画プロセスとの接続が終了しました。再起動できます。");
                        break;
                    }
                    Err(error) => {
                        sink.error(format!("描画プロセスの応答を読めません: {error}"));
                        break;
                    }
                },
                result = child.wait() => {
                    sink.error(format!("キャラクターが終了しました（{result:?}）。再起動できます。"));
                    break;
                }
            }
        }
        drop(client);
        if !matches!(timeout(REAP, child.wait()).await, Ok(Ok(_))) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if stop.is_cancelled() {
            return;
        }
        if !restart_requested && !wait_restart(&mut controls, &stop).await {
            return;
        }
    }
}

fn spawn(executable: &PathBuf) -> io::Result<Child> {
    Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
}

async fn wait_restart(controls: &mut mpsc::Receiver<Control>, stop: &CancellationToken) -> bool {
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return false,
            command = controls.recv() => match command {
                Some(Control::Restart) => return true,
                Some(Control::Recenter) => {}
                None => return false,
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Instant};

    const FIXTURE: &str = r#"#!/bin/sh
log='$LOG'
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"op":"add"'*)
      character=$(printf '%s' "$line" | sed -n 's/.*"character":"\([^"]*\)".*/\1/p')
      printf '{"event":"state","character":"%s","status":"ready","error":null}\n' "$character"
      printf '{"event":"click","character":"%s"}\n' "$character"
      ;;
  esac
done
"#;

    fn fixture(directory: &Path, body: &str) -> PathBuf {
        let log = directory.join("requests");
        let executable = directory.join("avatar");
        fs::write(
            &executable,
            body.replace("$LOG", &log.display().to_string()),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        log
    }

    fn requests(log: &Path) -> Vec<Value> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn request_ids(log: &Path, op: &str) -> Vec<u64> {
        requests(log)
            .iter()
            .filter(|request| request["op"] == op)
            .filter_map(|request| request["character"].as_str()?.parse().ok())
            .collect()
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "renderer fixture timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_ready(controller: &Controller) {
        wait_until(|| {
            controller
                .sink
                .inbox
                .lock()
                .unwrap()
                .state
                .as_ref()
                .is_some_and(|state| !state.loading)
        });
    }

    #[test]
    fn restart_reuses_the_latest_placement_and_discards_earlier_notifications() {
        let temporary = tempfile::tempdir().unwrap();
        let log = fixture(temporary.path(), FIXTURE);
        let controller =
            Controller::with_executable(temporary.path().join("avatar"), Arc::new(|| {}));
        controller.placement(AvatarPlacement {
            position: Some([80, 90]),
            monitor: Some("DP-1".into()),
        });
        controller.configure(AvatarPreferences::default());
        wait_ready(&controller);
        wait_until(|| request_ids(&log, "add").len() == 1);
        controller.restart();
        // The previous generation's click stays undrained and must not survive.
        wait_until(|| request_ids(&log, "add").len() == 2);
        wait_ready(&controller);
        let notifications = controller.drain();
        assert_eq!(notifications.events, vec![AvatarEvent::ShowChat]);
        assert_eq!(
            notifications.state.unwrap().placement,
            AvatarPlacement {
                position: Some([80, 90]),
                monitor: Some("DP-1".into())
            }
        );
        let adds = requests(&log);
        let adds: Vec<_> = adds
            .iter()
            .filter(|request| request["op"] == "add")
            .collect();
        assert_eq!(adds.len(), 2);
        for add in &adds {
            assert_eq!(add["config"]["position"], json!([80, 90]));
            assert_eq!(add["config"]["monitor"], json!("DP-1"));
        }
        assert!(
            adds[1]["character"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > adds[0]["character"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
        );
        controller.shutdown();
    }

    #[test]
    fn showing_again_uses_a_new_identity_after_removal() {
        let temporary = tempfile::tempdir().unwrap();
        let log = fixture(temporary.path(), FIXTURE);
        let controller =
            Controller::with_executable(temporary.path().join("avatar"), Arc::new(|| {}));
        controller.configure(AvatarPreferences::default());
        wait_ready(&controller);
        wait_until(|| request_ids(&log, "add").len() == 1);
        let hidden = AvatarPreferences {
            visible: false,
            ..AvatarPreferences::default()
        };
        controller.configure(hidden.clone());
        wait_until(|| !request_ids(&log, "remove").is_empty());
        controller.configure(AvatarPreferences::default());
        wait_until(|| request_ids(&log, "add").len() == 2);
        let adds = request_ids(&log, "add");
        assert!(adds[1] > adds[0]);
        assert!(
            controller
                .sink
                .inbox
                .lock()
                .unwrap()
                .events
                .iter()
                .all(|event| *event == AvatarEvent::ShowChat)
        );
        controller.shutdown();
    }

    #[test]
    fn shutdown_reaps_a_renderer_that_ignores_eof() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("started");
        let script = format!("#!/bin/sh\ntouch '{}'\nexec sleep 30\n", marker.display());
        let executable = temporary.path().join("avatar");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = Controller::with_executable(executable, Arc::new(|| {}));
        controller.configure(AvatarPreferences::default());
        wait_until(|| marker.exists());
        let stopping = Instant::now();
        controller.shutdown();
        assert!(stopping.elapsed() < Duration::from_secs(5));
        assert!(controller.worker.lock().unwrap().is_none());
    }
}
