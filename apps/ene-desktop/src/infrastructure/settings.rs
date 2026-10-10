//! Client preferences and serial, debounced persistence.
use atomic_write_file::{AtomicWriteFile, unix::OpenOptionsExt as _};
use ene_ipc::renderer::{AvatarConfig, AvatarPlacement};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
/// Local display preferences; only `config` is sent as an avatar contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AvatarPreferences {
    pub config: AvatarConfig,
    pub visible: bool,
    pub fps: u32,
}
impl Default for AvatarPreferences {
    fn default() -> Self {
        Self {
            config: AvatarConfig::default(),
            visible: true,
            fps: 30,
        }
    }
}
impl AvatarPreferences {
    pub fn validate(&self) -> Result<(), String> {
        self.config.validate()?;
        if ![30, 60].contains(&self.fps) {
            return Err("描画速度は30または60fpsを選んでください。".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    pub font_family: String,
    pub text_size: i32,
    pub theme_index: i32,
    pub avatar: AvatarPreferences,
    pub placement: AvatarPlacement,
}
impl Default for ClientSettings {
    fn default() -> Self {
        let mut avatar = AvatarPreferences::default();
        let cache = super::platform::cache();
        avatar.config.model = cache
            .join("models/Seed-san.vrm")
            .to_string_lossy()
            .into_owned();
        avatar.config.motion = Some(
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
            placement: AvatarPlacement::default(),
        }
    }
}
pub fn config_path() -> PathBuf {
    super::platform::config().join("client.json")
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
        preserve_invalid(path)?;
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

fn preserve_invalid(path: &Path) -> Result<(), String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("既存の設定を確認できません: {error}")),
    };
    if serde_json::from_slice::<ClientSettings>(&bytes)
        .is_ok_and(|settings| settings.avatar.validate().is_ok())
    {
        return Ok(());
    }
    // Creating a hard link never overwrites another backup. Keep the original
    // if preserving it fails; only then may the atomic writer replace it.
    for index in 0.. {
        let backup = path.with_file_name(format!("client.json.invalid-{index}"));
        match fs::hard_link(path, &backup) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("読めない設定を退避できません: {error}")),
        }
    }
    unreachable!()
}
#[derive(Clone)]
pub struct SaveResult {
    pub settings: ClientSettings,
    pub error: Option<String>,
}
pub struct Saver {
    updates: watch::Sender<ClientSettings>,
    retry: Arc<Notify>,
    stop: CancellationToken,
    result: Arc<Mutex<Option<SaveResult>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Saver {
    pub fn new(path: PathBuf, initial: ClientSettings, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (updates, incoming) = watch::channel(initial);
        let retry = Arc::new(Notify::new());
        let stop = CancellationToken::new();
        let result = Arc::new(Mutex::new(None));
        let retry_worker = retry.clone();
        let stop_worker = stop.clone();
        let result_worker = result.clone();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("settings runtime");
            runtime.block_on(save_loop(
                path,
                incoming,
                retry_worker,
                stop_worker,
                result_worker,
                wake,
            ));
        });
        Self {
            updates,
            retry,
            stop,
            result,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn update(&self, settings: ClientSettings) {
        self.updates.send_if_modified(|current| {
            if *current == settings {
                false
            } else {
                *current = settings;
                true
            }
        });
    }
    pub fn retry(&self) {
        self.retry.notify_one();
    }
    pub fn take_result(&self) -> Option<SaveResult> {
        self.result.lock().unwrap().take()
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Saver {
    fn drop(&mut self) {
        self.shutdown();
    }
}
async fn save_loop(
    path: PathBuf,
    mut incoming: watch::Receiver<ClientSettings>,
    retry: Arc<Notify>,
    stop: CancellationToken,
    result: Arc<Mutex<Option<SaveResult>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let mut dirty = false;
    let mut deadline = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => {
                if dirty || incoming.has_changed().unwrap_or(false) {
                    let snapshot = incoming.borrow_and_update().clone();
                    persist(&path, &snapshot, &result, &wake);
                }
                return;
            },
            changed = incoming.changed() => {
                if changed.is_err() { return; }
                dirty = true;
                deadline = Some(tokio::time::Instant::now() + Duration::from_millis(300));
            },
            _ = retry.notified() => {
                dirty = true;
                deadline = Some(tokio::time::Instant::now());
            },
            _ = async {
                if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
                else { std::future::pending::<()>().await; }
            } => {
                let snapshot = incoming.borrow_and_update().clone();
                dirty = !persist(&path, &snapshot, &result, &wake);
                deadline = None;
            },
        }
    }
}
fn persist(
    path: &Path,
    settings: &ClientSettings,
    result: &Mutex<Option<SaveResult>>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) -> bool {
    let error = settings.save(path).err();
    let success = error.is_none();
    *result.lock().unwrap() = Some(SaveResult {
        settings: settings.clone(),
        error,
    });
    wake();
    success
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ene-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    fn wait_result(saver: &Saver) -> SaveResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = saver.take_result() {
                return result;
            }
            assert!(std::time::Instant::now() < deadline, "save timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn shutdown_saves_latest_config_and_placement() {
        let directory = directory();
        let path = directory.join("client.json");
        let saver = Saver::new(path.clone(), ClientSettings::default(), Arc::new(|| {}));
        let mut expected = ClientSettings::default();
        for size in 50..=200 {
            expected.avatar.config.size_percent = size;
            expected.placement.x = size as i32;
            saver.update(expected.clone());
        }
        saver.shutdown();
        assert_eq!(ClientSettings::load(&path).unwrap(), expected);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn invalid_legacy_config_is_preserved_without_overwriting_an_existing_backup() {
        let directory = directory();
        let path = directory.join("client.json");
        let original = b"{\"avatar\":{\"right\":42,\"bottom\":12}}";
        fs::write(&path, original).unwrap();
        fs::write(directory.join("client.json.invalid-0"), "existing backup").unwrap();
        assert!(ClientSettings::load(&path).is_err());
        ClientSettings::default().save(&path).unwrap();
        assert_eq!(
            fs::read(directory.join("client.json.invalid-1")).unwrap(),
            original
        );
        assert_eq!(
            fs::read_to_string(directory.join("client.json.invalid-0")).unwrap(),
            "existing backup"
        );
        ClientSettings::load(&path).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn failed_save_can_be_retried_without_losing_live_settings() {
        let directory = directory();
        let path = directory.join("client.json");
        fs::create_dir(&path).unwrap();
        let saver = Saver::new(path.clone(), ClientSettings::default(), Arc::new(|| {}));
        let expected = ClientSettings {
            theme_index: 2,
            ..ClientSettings::default()
        };
        saver.update(expected.clone());
        let failed = wait_result(&saver);
        assert!(failed.error.is_some());
        assert_eq!(failed.settings, expected);
        fs::remove_dir(&path).unwrap();
        saver.retry();
        assert!(wait_result(&saver).error.is_none());
        assert_eq!(ClientSettings::load(&path).unwrap(), expected);
        saver.shutdown();
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn continuous_edits_debounce_to_one_latest_snapshot() {
        let directory = directory();
        let path = directory.join("client.json");
        let saver = Saver::new(path.clone(), ClientSettings::default(), Arc::new(|| {}));
        let mut expected = ClientSettings::default();
        for size in [110, 120, 130] {
            expected.avatar.config.size_percent = size;
            saver.update(expected.clone());
            std::thread::sleep(Duration::from_millis(80));
            assert!(!path.exists());
        }
        let result = wait_result(&saver);
        assert_eq!(result.settings, expected);
        assert!(result.error.is_none());
        assert_eq!(ClientSettings::load(&path).unwrap(), expected);
        saver.shutdown();
        fs::remove_dir_all(directory).unwrap();
    }
}
