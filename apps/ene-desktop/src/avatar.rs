use atomic_write_file::{AtomicWriteFile, unix::OpenOptionsExt as _};
use ene_protocol::avatar::{AvatarEvent, AvatarRequest, AvatarSettings};
use ene_protocol::ipc;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{mpsc as channel, watch},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub font_family: String,
    pub text_size: i32,
    pub theme_index: i32,
    pub avatar: AvatarSettings,
}
impl Default for ClientSettings {
    fn default() -> Self {
        let mut avatar = AvatarSettings::default();
        let cache = crate::platform::cache();
        avatar.model = cache
            .join("models/Seed-san.vrm")
            .to_string_lossy()
            .into_owned();
        avatar.motion = Some(
            cache
                .join("motions/three-vrm-sample.vrma")
                .to_string_lossy()
                .into_owned(),
        );
        Self {
            font_family: "Noto Sans CJK JP".into(),
            text_size: 16,
            theme_index: 0,
            avatar,
        }
    }
}
pub fn config_path() -> PathBuf {
    crate::platform::config().join("client.json")
}
impl ClientSettings {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut settings: Self =
                    serde_json::from_slice(&bytes).map_err(|e| format!("設定を読めません: {e}"))?;
                settings.avatar.validate()?;
                settings.theme_index = settings.theme_index.clamp(0, 2);
                settings.text_size = settings.text_size.clamp(12, 28);
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("設定を読めません: {e}")),
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.avatar.validate()?;
        let directory = path.parent().ok_or("設定フォルダがありません")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|e| format!("設定フォルダを作れません: {e}"))?;
        let result = (|| -> std::io::Result<()> {
            let mut file = AtomicWriteFile::options()
                .mode(0o600)
                .preserve_mode(false)
                .preserve_owner(false)
                .open(path)?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")?;
            file.commit()
        })();
        result.map_err(|e| format!("設定を保存できません: {e}"))
    }
}
enum Control {
    Recenter,
    Restart,
    Shutdown,
}
pub struct Controller {
    commands: channel::UnboundedSender<Control>,
    settings: watch::Sender<Option<AvatarSettings>>,
    pub events: Receiver<AvatarEvent>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Controller {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let executable = std::env::var_os("ENE_AVATAR_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_default()
                    .with_file_name("ene-avatar")
            });
        Self::with_notifier(executable, Some(Arc::new(wake)))
    }
    fn with_notifier(executable: PathBuf, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        let (commands, receiver) = channel::unbounded_channel();
        let (settings, updates) = watch::channel(None);
        let (sender, events) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("avatar runtime");
            runtime.block_on(supervise(
                executable,
                receiver,
                updates,
                EventSink { sender, wake },
            ));
        });
        Self {
            commands,
            settings,
            events,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn configure(&self, settings: AvatarSettings) {
        self.settings.send_replace(Some(settings));
    }
    pub fn restart(&self, settings: AvatarSettings) {
        self.configure(settings);
        let _ = self.commands.send(Control::Restart);
    }
    pub fn recenter(&self) {
        let _ = self.commands.send(Control::Recenter);
    }
    pub fn shutdown(&self) {
        let _ = self.commands.send(Control::Shutdown);
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
struct EventSink {
    sender: Sender<AvatarEvent>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl EventSink {
    fn send(&self, event: AvatarEvent) {
        let _ = self.sender.send(event);
        if let Some(wake) = &self.wake {
            wake();
        }
    }
    fn error(&self, error: impl std::fmt::Display) {
        self.send(AvatarEvent::Error {
            message: error.to_string(),
        });
    }
}
async fn write_requests(
    input: tokio::process::ChildStdin,
    mut settings: watch::Receiver<AvatarSettings>,
    mut centers: channel::Receiver<()>,
    stop: CancellationToken,
) -> Result<(), ipc::Error> {
    let mut output = ipc::Writer::new(input);
    let mut request = AvatarRequest::Configure {
        settings: settings.borrow_and_update().clone(),
    };
    loop {
        // Cancelling a partially sent line drops the pipe. It is never reused.
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(()),
            result = output.send(&request) => result?,
        }
        request = tokio::select! {
            biased;
            _ = stop.cancelled() => {
                // This branch is reached between complete writes.
                tokio::time::timeout(Duration::from_millis(100), output.send(&AvatarRequest::Shutdown)).await.ok();
                return Ok(());
            }
            result = settings.changed() => {
                if result.is_err() { return Ok(()); }
                AvatarRequest::Configure { settings: settings.borrow_and_update().clone() }
            }
            Some(()) = centers.recv() => AvatarRequest::Recenter,
        };
    }
}
async fn supervise(
    executable: PathBuf,
    mut controls: channel::UnboundedReceiver<Control>,
    mut updates: watch::Receiver<Option<AvatarSettings>>,
    sink: EventSink,
) {
    let mut pending = None;
    let mut center = false;
    loop {
        while pending.is_none() {
            tokio::select! {
                biased;
                command = controls.recv() => match command {
                    None | Some(Control::Shutdown) => return,
                    Some(Control::Recenter) => center = true,
                    Some(Control::Restart) => pending = updates.borrow_and_update().clone(),
                },
                changed = updates.changed() => {
                    if changed.is_err() { return; }
                    pending = updates.borrow_and_update().clone();
                }
            }
        }
        let settings = pending.take().unwrap();
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
                continue;
            }
        };
        let (configuration, incoming) = watch::channel(settings);
        let (centers, outgoing) = channel::channel(1);
        if center {
            let _ = centers.try_send(());
            center = false;
        }
        let stop = CancellationToken::new();
        let mut writer = tokio::spawn(write_requests(
            child.stdin.take().unwrap(),
            incoming,
            outgoing,
            stop.clone(),
        ));
        let mut reader = ipc::Reader::new(child.stdout.take().unwrap());
        let mut quitting = false;
        let mut exited = false;
        let mut writer_finished = false;
        loop {
            tokio::select! {
                biased;
                command = controls.recv() => match command {
                    None | Some(Control::Shutdown) => { quitting = true; break; }
                    Some(Control::Restart) => { pending = updates.borrow_and_update().clone(); break; }
                    Some(Control::Recenter) => { let _ = centers.try_send(()); }
                },
                result = child.wait() => {
                    exited = result.is_ok();
                    match result { Ok(status) => sink.error(format!("キャラクターが終了しました（{status}）。再起動できます。")), Err(error) => sink.error(error) }
                    break;
                }
                result = &mut writer => {
                    writer_finished = true;
                    match result { Ok(Err(error)) => sink.error(error), Err(error) => sink.error(error), _ => {} }
                    break;
                }
                event = reader.receive::<AvatarEvent>() => match event {
                    Ok(Some(event)) => sink.send(event),
                    Ok(None) => { sink.error("描画IPCが切断されました。"); break; }
                    Err(error) => { sink.error(error); break; }
                },
                changed = updates.changed() => {
                    if changed.is_err() { quitting = true; break; }
                    if let Some(settings) = updates.borrow_and_update().clone() { configuration.send_replace(settings); }
                }
            }
        }
        // The deadline includes any pending pipe write; shutdown never queues
        // behind configuration. EOF also releases the renderer's input wait.
        stop.cancel();
        if !exited
            && !matches!(
                tokio::time::timeout(Duration::from_secs(3), child.wait()).await,
                Ok(Ok(_))
            )
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if !writer_finished {
            writer.abort();
            let _ = writer.await;
        }
        if quitting {
            return;
        }
        // A shutdown received during restart prevents spawning another child.
        while let Ok(command) = controls.try_recv() {
            match command {
                Control::Shutdown => return,
                Control::Restart => pending = updates.borrow_and_update().clone(),
                Control::Recenter => center = true,
            }
        }
        if pending.is_some() {
            pending = updates.borrow_and_update().clone();
        }
    }
}
