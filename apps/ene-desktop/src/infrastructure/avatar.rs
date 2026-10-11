//! Child-process lifecycle and bounded notifications. Preferences live in settings.
use super::settings::AvatarPreferences;
use ene_ipc::{
    remoc::{self, Cfg, Connect, ConnectExt, rch},
    renderer::{
        AvatarEvent, AvatarId, AvatarInstance, AvatarPlacement, AvatarState, RendererClient,
        RendererConfig, RendererMessage, RendererRequest,
    },
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

const CAPACITY: usize = 64;
#[derive(Default)]
struct Inbox {
    events: VecDeque<AvatarEvent>,
    error: Option<String>,
    notified: bool,
}
#[derive(Clone)]
struct Sink {
    id: AvatarId,
    states: watch::Sender<Option<AvatarState>>,
    inbox: Arc<Mutex<Inbox>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Sink {
    fn send(&self, message: RendererMessage) -> Result<(), &'static str> {
        let mut inbox = self.inbox.lock().unwrap();
        match message {
            RendererMessage::Event { avatar_id, event } if avatar_id == self.id => {
                if inbox.events.len() == CAPACITY {
                    return Err("描画イベントのキューが満杯です。");
                }
                inbox.events.push_back(event);
            }
            RendererMessage::Error { avatar_id, message }
                if avatar_id.is_none_or(|id| id == self.id) =>
            {
                inbox.error = Some(message);
            }
            _ => return Ok(()),
        }
        self.notify(&mut inbox);
        Ok(())
    }
    fn state(&self, state: Option<AvatarState>) {
        self.states.send_replace(state);
        self.notify(&mut self.inbox.lock().unwrap());
    }
    fn error(&self, message: impl std::fmt::Display) {
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
    fn reset(&self) {
        let mut inbox = self.inbox.lock().unwrap();
        *inbox = Inbox::default();
        self.states.send_replace(None);
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
    states: Mutex<watch::Receiver<Option<AvatarState>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Controller {
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let executable = std::env::var_os("ENE_AVATAR_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_default()
                    .with_file_name(ene_platform::executable("ene-avatar"))
            });
        Self::with_executable(executable, wake)
    }
    fn with_executable(executable: PathBuf, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (commands, controls) = mpsc::channel(CAPACITY);
        let (config, updates) = watch::channel(None);
        let (placement, _) = watch::channel(AvatarPlacement::default());
        let stop = CancellationToken::new();
        let (states, incoming_states) = watch::channel(None);
        let sink = Sink {
            id: AvatarId::new(),
            states,
            inbox: Arc::new(Mutex::new(Inbox::default())),
            wake,
        };
        let worker_sink = sink.clone();
        let worker_placement = placement.clone();
        let worker_stop = stop.clone();
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
            states: Mutex::new(incoming_states),
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
        self.placement.send_replace(placement);
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
        let mut states = self.states.lock().unwrap();
        let state = if states.has_changed().unwrap_or(false) {
            states.borrow_and_update().clone()
        } else {
            None
        };
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
async fn write_requests(
    output: rch::mpsc::Sender<RendererRequest>,
    initial: AvatarPreferences,
    avatar_id: AvatarId,
    placement: watch::Receiver<AvatarPlacement>,
    mut config: watch::Receiver<Option<AvatarPreferences>>,
    mut commands: mpsc::Receiver<()>,
    stop: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut visible = initial.visible;
    let mut previous = initial.clone();
    let mut pending = VecDeque::from([RendererRequest::Initialize {
        config: RendererConfig { fps: initial.fps },
        avatars: if visible {
            vec![AvatarInstance {
                id: avatar_id,
                config: initial.config,
                placement: placement.borrow().clone(),
            }]
        } else {
            vec![]
        },
    }]);
    loop {
        while let Some(request) = pending.pop_front() {
            tokio::select! { biased; _ = stop.cancelled() => return Ok(()), result = async { output.send(request).await?.await?; Ok::<_, Box<dyn std::error::Error + Send + Sync>>(()) } => result? }
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(()),
            Some(()) = commands.recv() => { if visible { pending.push_back(RendererRequest::Recenter { avatar_id }); } },
            changed = config.changed() => {
                if changed.is_err() { return Ok(()); }
                let Some(config) = config.borrow_and_update().clone() else { return Ok(()); };
                if config.fps != previous.fps { pending.push_back(RendererRequest::SetRenderConfig { config: RendererConfig { fps: config.fps } }); }
                if config.visible != visible {
                    visible = config.visible;
                    pending.push_back(if visible {
                        RendererRequest::AddAvatar { avatar: AvatarInstance { id: avatar_id, config: config.config.clone(), placement: placement.borrow().clone() } }
                    } else { RendererRequest::RemoveAvatar { avatar_id } });
                } else if visible && config.config != previous.config {
                    pending.push_back(RendererRequest::SetAvatarConfig { avatar_id, config: config.config.clone() });
                }
                previous = config;
            }
        }
    }
}
async fn supervise(
    executable: PathBuf,
    mut controls: mpsc::Receiver<Control>,
    mut config: watch::Receiver<Option<AvatarPreferences>>,
    placement: watch::Sender<AvatarPlacement>,
    stop: CancellationToken,
    sink: Sink,
) {
    loop {
        let initial = loop {
            if let Some(initial) = config.borrow_and_update().clone() {
                break initial;
            }
            tokio::select! { biased; _ = stop.cancelled() => return, changed = config.changed() => { if changed.is_err() { return; } } }
        };
        if stop.is_cancelled() {
            return;
        }
        sink.reset();
        let mut child = match Command::new(&executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                sink.error(format!("キャラクターを起動できません: {error}"));
                if !wait_restart(&mut controls, &stop).await {
                    return;
                }
                continue;
            }
        };
        let mut restart_requested = false;
        let connecting =
            Connect::io::<_, _, RendererClient, RendererClient, remoc::codec::Default>(
                Cfg::default(),
                child.stdout.take().unwrap(),
                child.stdin.take().unwrap(),
            )
            .consume();
        tokio::pin!(connecting);
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        let connected = loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break None,
                command = controls.recv() => match command {
                    Some(Control::Restart) => { restart_requested = true; break None; }
                    Some(Control::Recenter) => {},
                    None => { stop.cancel(); break None; }
                },
                _ = &mut deadline => { sink.error("描画プロセスの接続がタイムアウトしました。再起動できます。"); break None; }
                result = &mut connecting => break Some(result),
            }
        };
        let RendererClient {
            requests: sender,
            messages: mut reader,
            states: mut snapshots,
        } = match connected {
            Some(Ok(connection)) => connection,
            result => {
                if let Some(Err(error)) = result {
                    sink.error(error);
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                if stop.is_cancelled()
                    || (!restart_requested && !wait_restart(&mut controls, &stop).await)
                {
                    return;
                }
                continue;
            }
        };
        let (centers, commands) = mpsc::channel(CAPACITY);
        let writer_stop = CancellationToken::new();
        let mut writer = tokio::spawn(write_requests(
            sender,
            initial,
            sink.id,
            placement.subscribe(),
            config.clone(),
            commands,
            writer_stop.clone(),
        ));
        let mut restart = false;
        let mut writer_finished = false;
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                command = controls.recv() => match command {
                    Some(Control::Restart) => { restart = true; break; },
                    Some(Control::Recenter) => { if centers.try_send(()).is_err() { sink.error("描画操作のキューが満杯です。"); break; } },
                    None => return,
                },
                result = child.wait() => { sink.error(format!("キャラクターが終了しました（{result:?}）。再起動できます。")); break; },
                result = &mut writer => { writer_finished = true; if let Err(error) = result { sink.error(error); } else if let Ok(Err(error)) = result { sink.error(error); } break; },
                changed = snapshots.changed() => {
                    if let Err(error) = changed { sink.error(error); break; }
                    let snapshot = match snapshots.borrow_and_update() {
                        Ok(snapshot) => snapshot,
                        Err(error) => { sink.error(error); break; },
                    };
                    let state = snapshot.avatars.get(&sink.id).cloned();
                    if let Some(state) = &state {
                        placement.send_replace(state.placement.clone());
                    }
                    sink.state(state);
                },
                message = reader.recv() => match message {
                    Ok(Some(message)) => {
                        if let Err(error) = sink.send(message) { sink.error(error); break; }
                    },
                    Ok(None) => { sink.error("描画IPCが切断されました。"); break; },
                    Err(error) => { sink.error(error); break; },
                }
            }
        }
        writer_stop.cancel();
        if !writer_finished {
            writer.abort();
            let _ = writer.await;
        }
        // Dropping all client channel halves lets Remoc close the child's stdin.
        drop(reader);
        drop(snapshots);
        if !matches!(
            tokio::time::timeout(Duration::from_secs(3), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if stop.is_cancelled() {
            return;
        }
        if !restart && !wait_restart(&mut controls, &stop).await {
            return;
        }
    }
}
async fn wait_restart(controls: &mut mpsc::Receiver<Control>, stop: &CancellationToken) -> bool {
    loop {
        tokio::select! { biased; _ = stop.cancelled() => return false,
            command = controls.recv() => match command { Some(Control::Restart) => return true, Some(Control::Recenter) => {}, None => return false }
        }
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use ene_ipc::renderer::RendererState;
    use std::{fs, os::unix::fs::PermissionsExt, time::Instant};
    #[test]
    #[ignore = "Rust child-process fixture, launched by the restart test"]
    fn renderer_peer() {
        use tokio::net::unix::pipe;
        let directory = PathBuf::from(std::env::var_os("ENE_IPC_TEST_DIRECTORY").unwrap());
        if directory.join("stall-handshake").exists() {
            fs::write(directory.join("started"), "").unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let model = runtime.block_on(async {
            let output = pipe::Sender::from_file(
                fs::OpenOptions::new()
                    .write(true)
                    .open("/proc/self/fd/3")
                    .unwrap(),
            )
            .unwrap();
            let (sender, mut requests) = rch::mpsc::channel();
            let (messages, events) = rch::mpsc::channel();
            let (states, snapshots) = rch::watch::channel(RendererState::default());
            Connect::io::<_, _, RendererClient, RendererClient, remoc::codec::Default>(
                Cfg::default(),
                tokio::io::stdin(),
                output,
            )
            .provide(RendererClient {
                requests: sender,
                messages: events,
                states: snapshots,
            })
            .await
            .unwrap();
            let initial = requests.recv().await.unwrap().unwrap();
            let RendererRequest::Initialize { ref avatars, .. } = initial else {
                panic!("initialize")
            };
            let id = avatars[0].id;
            use std::io::Write;
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("requests"))
                .unwrap();
            writeln!(log, "{}", serde_json::to_string(&initial).unwrap()).unwrap();
            let marker = directory.join("started");
            let model = if marker.exists() {
                "second"
            } else {
                fs::write(&marker, "").unwrap();
                "first"
            };
            states.send_modify(|states| {
                states.avatars.insert(
                    id,
                    AvatarState {
                        placement: AvatarPlacement {
                            x: 80,
                            y: 90,
                            monitor: None,
                        },
                        loading: false,
                        model: Some(model.into()),
                        ..Default::default()
                    },
                );
            });
            messages
                .send(RendererMessage::Event {
                    avatar_id: id,
                    event: AvatarEvent::MotionFinished,
                })
                .await
                .unwrap();
            // The renderer also exits when the parent closes the connection.
            while let Ok(Some(request)) = requests.recv().await {
                if matches!(request, RendererRequest::Shutdown) {
                    break;
                }
            }
            model
        });
        drop(runtime);
        fs::write(directory.join(format!("{model}-exited")), "").unwrap();
    }
    #[test]
    fn restart_initializes_with_latest_placement_and_discards_previous_notifications() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let executable = directory.join("avatar");
        let log = directory.join("requests");
        // Give the Rust fixture its own stdout pipe on fd 3; libtest output
        // goes to stderr so it cannot corrupt the Remoc protocol.
        let script = format!(
            "#!/bin/sh\nENE_IPC_TEST_DIRECTORY='{}' exec '{}' --ignored --exact infrastructure::avatar::process_tests::renderer_peer --nocapture 3>&1 1>&2\n",
            directory.display(),
            std::env::current_exe().unwrap().display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = Controller::with_executable(executable, Arc::new(|| {}));
        controller.configure(AvatarPreferences::default());
        let wait_model = |model: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while controller
                .sink
                .states
                .borrow()
                .as_ref()
                .is_none_or(|state| state.model.as_deref() != Some(model))
            {
                assert!(Instant::now() < deadline, "child state timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
            // Leave first-generation notifications undrained until after restart.
            while controller.sink.inbox.lock().unwrap().events.is_empty() {
                assert!(Instant::now() < deadline, "child event timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_model("first");
        controller.restart();
        wait_model("second");
        assert!(directory.join("first-exited").exists());
        let notifications = controller.drain();
        assert_eq!(
            notifications.state.unwrap().model.as_deref(),
            Some("second")
        );
        assert_eq!(notifications.events.len(), 1);
        controller.shutdown();
        assert!(directory.join("second-exited").exists());
        let requests: Vec<RendererRequest> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let RendererRequest::Initialize { avatars: first, .. } = &requests[0] else {
            panic!("first initialize")
        };
        let RendererRequest::Initialize {
            avatars: second, ..
        } = &requests[1]
        else {
            panic!("second initialize")
        };
        assert_eq!(first[0].id, second[0].id);
        assert_eq!(second[0].placement.x, 80);
        assert_eq!(second[0].placement.y, 90);
    }

    #[test]
    fn shutdown_cancels_handshake_and_reaps_unresponsive_child() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        fs::write(directory.join("stall-handshake"), "").unwrap();
        let executable = directory.join("avatar");
        let script = format!(
            "#!/bin/sh\nENE_IPC_TEST_DIRECTORY='{}' exec '{}' --ignored --exact infrastructure::avatar::process_tests::renderer_peer --nocapture 3>&1 1>&2\n",
            directory.display(),
            std::env::current_exe().unwrap().display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = Controller::with_executable(executable, Arc::new(|| {}));
        controller.configure(AvatarPreferences::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !directory.join("started").exists() {
            assert!(Instant::now() < deadline, "child did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let stopping = Instant::now();
        controller.shutdown();
        assert!(stopping.elapsed() < Duration::from_secs(2));
        assert!(controller.worker.lock().unwrap().is_none());
    }
    #[test]
    fn restart_cancels_handshake_and_starts_a_new_child() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let executable = directory.join("avatar");
        let pids = directory.join("pids");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\n' \"$$\" >> '{}'\nexec sleep 30\n",
                pids.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = Controller::with_executable(executable, Arc::new(|| {}));
        controller.configure(AvatarPreferences::default());
        let started = |count: usize| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let pids: Vec<u32> = fs::read_to_string(&pids)
                    .unwrap_or_default()
                    .lines()
                    .map(|pid| pid.parse().unwrap())
                    .collect();
                if pids.len() >= count {
                    break pids;
                }
                assert!(Instant::now() < deadline, "child did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let first = started(1)[0];
        controller.restart();
        let second = started(2)[1];
        assert_ne!(first, second);
        assert!(!PathBuf::from(format!("/proc/{first}")).exists());
        controller.shutdown();
        assert!(!PathBuf::from(format!("/proc/{second}")).exists());
    }
}
