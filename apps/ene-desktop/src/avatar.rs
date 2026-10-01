use ene_shared::avatar::{AvatarEvent, AvatarRequest, AvatarSettings, MAX_IPC_LINE};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

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
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache"));
        avatar.model = cache
            .join("ene/models/Seed-san.vrm")
            .to_string_lossy()
            .into_owned();
        avatar.motion = Some(
            cache
                .join("ene/motions/three-vrm-sample.vrma")
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
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("ene/client.json")
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
        fs::create_dir_all(directory).map_err(|e| format!("設定フォルダを作れません: {e}"))?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut created = false;
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            created = true;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|e| format!("設定を保存できません: {e}"))
    }
}
enum Control {
    Configure(AvatarSettings),
    Restart(AvatarSettings),
    Shutdown,
}
pub struct Controller {
    commands: Sender<Control>,
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
        let (commands, receiver) = mpsc::channel();
        let (sender, events) = mpsc::channel();
        let worker =
            thread::spawn(move || supervise(executable, receiver, EventSink { sender, wake }));
        Self {
            commands,
            events,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn configure(&self, settings: AvatarSettings) {
        let _ = self.commands.send(Control::Configure(settings));
    }
    pub fn restart(&self, settings: AvatarSettings) {
        let _ = self.commands.send(Control::Restart(settings));
    }
}
impl Controller {
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
struct Process {
    child: Child,
    input: ChildStdin,
    events: Receiver<AvatarEvent>,
    reader: JoinHandle<()>,
    stopping: Option<Instant>,
}
impl Process {
    fn start(executable: &Path, settings: AvatarSettings) -> Result<Self, String> {
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!("キャラクターを起動できません: {e}（ene-avatarをビルドしてください）")
            })?;
        let input = child.stdin.take().unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: input owns a live pipe FD; only its file status flags change.
        let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: the owned pipe is still live, and O_NONBLOCK prevents a stuck
        // renderer from blocking the supervisor's independent shutdown deadline.
        if flags < 0
            || unsafe { libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("描画IPCを非同期にできません。".into());
        }
        let stdout = child.stdout.take().unwrap();
        let (sender, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take((MAX_IPC_LINE + 1) as u64)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() <= MAX_IPC_LINE => match serde_json::from_slice(&line) {
                        Ok(event) => {
                            if sender.send(event).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = sender.send(AvatarEvent::Error {
                                message: format!("描画プロセスの応答が不正です: {e}"),
                            });
                            break;
                        }
                    },
                    _ => {
                        let _ = sender.send(AvatarEvent::Error {
                            message: "描画プロセスの応答が大きすぎます。".into(),
                        });
                        break;
                    }
                }
            }
        });
        let mut process = Self {
            child,
            input,
            events,
            reader,
            stopping: None,
        };
        if let Err(error) = process.send(AvatarRequest::Configure { settings }) {
            process.kill_and_reap();
            return Err(error);
        }
        Ok(process)
    }
    fn send(&mut self, request: AvatarRequest) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, &request).map_err(|e| e.to_string())?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|e| e.to_string())
    }
    fn stop(&mut self) {
        if self.stopping.is_none() {
            self.stopping = Some(Instant::now());
            let _ = self.send(AvatarRequest::Shutdown);
        }
    }
    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct EventSink {
    sender: Sender<AvatarEvent>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl EventSink {
    fn send(&self, event: AvatarEvent) -> Result<(), mpsc::SendError<AvatarEvent>> {
        self.sender.send(event)?;
        if let Some(wake) = &self.wake {
            wake();
        }
        Ok(())
    }
}
fn supervise(executable: PathBuf, receiver: Receiver<Control>, sender: EventSink) {
    let mut process = None::<Process>;
    let mut pending = None;
    let mut quitting = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Control::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                quitting = true;
                pending = None;
                if let Some(p) = process.as_mut() {
                    p.stop();
                }
            }
            Ok(Control::Restart(settings)) => {
                pending = Some(settings);
                if let Some(p) = process.as_mut() {
                    p.stop();
                }
            }
            Ok(Control::Configure(settings)) => {
                if let Some(p) = process.as_mut() {
                    if p.stopping.is_some() {
                        pending = Some(settings);
                    } else if let Err(error) = p.send(AvatarRequest::Configure { settings }) {
                        let _ = sender.send(AvatarEvent::Error { message: error });
                        p.stop();
                    }
                } else {
                    pending = Some(settings);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(p) = process.as_mut() {
            for event in p.events.try_iter() {
                let _ = sender.send(event);
            }
            if p.stopping
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(3))
            {
                p.kill_and_reap();
            }
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    if p.stopping.is_none() {
                        let _ = sender.send(AvatarEvent::Error {
                            message: format!(
                                "キャラクターが終了しました（{status}）。再起動できます。"
                            ),
                        });
                    }
                    let p = process.take().unwrap();
                    let _ = p.reader.join();
                    // Drain any final error produced just before exit.
                    for event in p.events.try_iter() {
                        let _ = sender.send(event);
                    }
                }
                Err(e) => {
                    let _ = sender.send(AvatarEvent::Error {
                        message: e.to_string(),
                    });
                    p.kill_and_reap();
                    let p = process.take().unwrap();
                    let _ = p.reader.join();
                }
                _ => {}
            }
        }
        if process.is_none() {
            if quitting {
                break;
            }
            if let Some(settings) = pending.take() {
                match Process::start(&executable, settings) {
                    Ok(p) => process = Some(p),
                    Err(error) => {
                        let _ = sender.send(AvatarEvent::Error { message: error });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn saved_configuration_preserves_unicode_position_and_permissions() {
        let directory = std::env::temp_dir().join(format!("ene-settings-{}", std::process::id()));
        let path = directory.join("client.json");
        let mut settings = ClientSettings::default();
        settings.avatar.model = "/tmp/日本語モデル.vrm".into();
        settings.avatar.motion = Some("/tmp/挨拶.vrma".into());
        settings.avatar.motion_playing = false;
        settings.avatar.motion_loop = false;
        settings.theme_index = 2;
        settings.avatar.size_percent = 150;
        settings.avatar.right = 123;
        settings.avatar.output = Some("DP-2".into());
        settings.save(&path).unwrap();
        let loaded = ClientSettings::load(&path).unwrap();
        assert_eq!(loaded.avatar, settings.avatar);
        assert_eq!(loaded.theme_index, settings.theme_index);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
