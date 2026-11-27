//! Local VOICEVOX Engine process discovery, lifecycle management and readiness probing.

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

const DEFAULT_PORT: u16 = 50021;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(25);
const POLL_INTERVAL: Duration = Duration::from_millis(300);

#[derive(Default)]
struct ManagedProcess {
    endpoint: String,
    child: Option<Box<dyn ChildWrapper>>,
}

#[derive(Debug, thiserror::Error)]
pub enum VoicevoxError {
    #[error("VOICEVOX自動起動はテスト実行中はスキップされます")]
    SkippedInTests,
    #[error("リモートのVOICEVOX接続先は自動起動できません")]
    RemoteEndpointNotAutoStartable,
    #[error(
        "VOICEVOXエンジンが見つかりません。VOICEVOXをインストールするか、環境変数 ENE_VOICEVOX_ENGINE に run.exe のパスを設定してください。"
    )]
    EngineBinaryNotFound,
    #[error("VOICEVOXエンジンが起動直後に終了しました（code: {0}）")]
    ExitedEarly(std::process::ExitStatus),
    #[error("VOICEVOXエンジンの起動待機がタイムアウトしました（25秒）")]
    StartupTimeout,
    #[error("VOICEVOXエンジンの起動に失敗しました ({path}): {error}")]
    SpawnFailed { path: String, error: String },
}

/// Owns at most one locally spawned VOICEVOX Engine child process.
#[derive(Clone)]
pub struct VoicevoxManager {
    inner: Arc<Mutex<ManagedProcess>>,
    auto_start: bool,
}

impl Default for VoicevoxManager {
    fn default() -> Self {
        Self::with_auto_start(true)
    }
}

impl fmt::Debug for VoicevoxManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoicevoxManager")
            .field("auto_start", &self.auto_start)
            .finish_non_exhaustive()
    }
}

impl VoicevoxManager {
    /// Creates a manager with configurable local process auto-start behavior.
    pub fn with_auto_start(auto_start: bool) -> Self {
        Self {
            inner: Arc::default(),
            auto_start,
        }
    }

    /// Ensures a local VOICEVOX Engine at `base_url` is reachable, launching it if needed.
    pub async fn ensure_running(
        &self,
        base_url: &str,
        options_json: &str,
    ) -> Result<(), VoicevoxError> {
        let base = normalized_voicevox_base(base_url);
        if (!self.auto_start || cfg!(test)) && std::env::var_os("ENE_VOICEVOX_ENGINE").is_none() {
            return Err(VoicevoxError::SkippedInTests);
        }
        let mut guard = self.inner.lock().await;
        if is_reachable(base, Duration::from_millis(1_200)).await {
            return Ok(());
        }
        let Some((host, port)) = is_local_voicevox_url(base) else {
            return Err(VoicevoxError::RemoteEndpointNotAutoStartable);
        };
        if !same_process_running(&mut guard, base) {
            start_local_engine(&mut guard, base, &host, port, options_json).await?;
        }
        wait_for_engine_ready(&mut guard, base).await
    }

    /// Stops any VOICEVOX Engine child process managed by this host.
    pub async fn stop(&self) {
        let mut guard = self.inner.lock().await;
        stop_inner(&mut guard).await;
    }
}

fn normalized_voicevox_base(base_url: &str) -> &str {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        "http://127.0.0.1:50021"
    } else {
        base
    }
}

fn same_process_running(managed: &mut ManagedProcess, base: &str) -> bool {
    if managed.endpoint != base {
        return false;
    }
    managed
        .child
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
}

async fn start_local_engine(
    managed: &mut ManagedProcess,
    base: &str,
    host: &str,
    port: u16,
    options_json: &str,
) -> Result<(), VoicevoxError> {
    stop_inner(managed).await;
    let exe = find_engine_binary(options_json).ok_or(VoicevoxError::EngineBinaryNotFound)?;
    tracing::info!(engine = %exe.display(), host = %host, port, "starting local VOICEVOX engine");
    let child = spawn_engine(&exe, host, port)?;
    managed.endpoint = base.to_owned();
    managed.child = Some(child);
    Ok(())
}

async fn wait_for_engine_ready(
    managed: &mut ManagedProcess,
    base: &str,
) -> Result<(), VoicevoxError> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if is_reachable(base, Duration::from_millis(1_000)).await {
            tracing::info!(endpoint = %base, "VOICEVOX engine is ready");
            return Ok(());
        }
        if let Some(child) = managed.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            managed.child = None;
            return Err(VoicevoxError::ExitedEarly(status));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(VoicevoxError::StartupTimeout)
}

async fn stop_inner(managed: &mut ManagedProcess) {
    if let Some(mut child) = managed.child.take() {
        ene_domain::debug_if_err(
            Box::into_pin(child.kill()).await,
            "voicevox child termination",
        );
        ene_domain::debug_if_err(child.wait().await, "voicevox child reap");
    }
    managed.endpoint.clear();
}

fn spawn_engine(exe: &Path, host: &str, port: u16) -> Result<Box<dyn ChildWrapper>, VoicevoxError> {
    let port_arg = port.to_string();
    let mut wrap = CommandWrap::with_new(exe, |cmd| {
        cmd.args(["--host", host, "--port", &port_arg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    });
    #[cfg(unix)]
    {
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    }
    #[cfg(windows)]
    {
        wrap.wrap(process_wrap::tokio::JobObject);
    }
    wrap.wrap(KillOnDrop)
        .spawn()
        .map_err(|error| VoicevoxError::SpawnFailed {
            path: exe.display().to_string(),
            error: error.to_string(),
        })
}

pub async fn is_reachable(base_url: &str, timeout: Duration) -> bool {
    // One client for every poll, so the connection pool survives between them.
    // The deadline is per request because each caller has its own budget.
    static CLIENT: std::sync::LazyLock<Option<reqwest::Client>> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()
    });
    let Some(client) = CLIENT.as_ref() else {
        return false;
    };
    let url = format!("{}/speakers", base_url.trim_end_matches('/'));
    client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .is_ok_and(|res| res.status().is_success())
}

/// Parses a loopback HTTP URL for VOICEVOX (`http://127.0.0.1:50021`, `http://localhost:50021`, etc.).
pub fn is_local_voicevox_url(base: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(base.trim()).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    let host = parsed.host_str()?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || parsed.host().is_some_and(|h| match h {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        });
    if !is_loopback {
        return None;
    }
    let bind_host = match parsed.host() {
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        _ if host.eq_ignore_ascii_case("localhost") => "127.0.0.1".to_owned(),
        _ => host.to_owned(),
    };
    let port = parsed.port().unwrap_or(DEFAULT_PORT);
    Some((bind_host, port))
}

/// Resolves the VOICEVOX Engine binary across explicit options, env vars, and standard install locations.
pub fn find_engine_binary(options_json: &str) -> Option<PathBuf> {
    if let Some(path) = find_explicit_or_env_binary(options_json) {
        return Some(path);
    }
    #[cfg(windows)]
    {
        find_windows_binary()
    }
    #[cfg(target_os = "macos")]
    {
        find_macos_binary()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        find_linux_binary()
    }
}

fn find_explicit_or_env_binary(options_json: &str) -> Option<PathBuf> {
    if !options_json.trim().is_empty()
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(options_json)
        && let Some(raw) = val.get("engine_path").and_then(|v| v.as_str())
    {
        let path = PathBuf::from(raw.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    for var in ["ENE_VOICEVOX_ENGINE", "VOICEVOX_ENGINE_PATH"] {
        if let Some(raw) = std::env::var_os(var) {
            let path = PathBuf::from(raw);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(windows)]
fn find_windows_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local) = directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().to_owned()) {
        candidates.push(
            local
                .join("Programs")
                .join("VOICEVOX")
                .join("vv-engine")
                .join("run.exe"),
        );
        candidates.push(local.join("VOICEVOX").join("vv-engine").join("run.exe"));
        let winget_dir = local.join("Microsoft").join("WinGet").join("Packages");
        if let Ok(entries) = std::fs::read_dir(&winget_dir) {
            let mut packages: Vec<(bool, String, PathBuf)> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("HiroshibaKazuyuki.VOICEVOX") {
                    let exe = entry
                        .path()
                        .join("VOICEVOX")
                        .join("vv-engine")
                        .join("run.exe");
                    if exe.is_file() {
                        let is_cpu = name.contains(".CPU");
                        packages.push((is_cpu, name, exe));
                    }
                }
            }
            // Prefer CPU build on Windows so GPU-less/AMD/Intel machines start reliably without CUDA DLL failures.
            packages.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            for (_, _, exe) in packages {
                candidates.push(exe);
            }
        }
    }
    if let Some(pf) = std::env::var_os("ProgramFiles").map(PathBuf::from) {
        candidates.push(pf.join("VOICEVOX").join("vv-engine").join("run.exe"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| find_on_path("voicevox_engine.exe"))
}

#[cfg(target_os = "macos")]
fn find_macos_binary() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from(
        "/Applications/VOICEVOX.app/Contents/Resources/vv-engine/run",
    )];
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_owned()) {
        candidates.push(
            home.join("Applications")
                .join("VOICEVOX.app")
                .join("Contents")
                .join("Resources")
                .join("vv-engine")
                .join("run"),
        );
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| find_on_path("voicevox_engine"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_linux_binary() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/voicevox/vv-engine/run"),
        PathBuf::from("/usr/local/bin/voicevox_engine"),
    ];
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_owned()) {
        candidates.push(
            home.join(".local")
                .join("share")
                .join("voicevox")
                .join("vv-engine")
                .join("run"),
        );
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| find_on_path("voicevox_engine"))
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_paths(binary, &path)
}

/// The PATH search, taking the path list explicitly so a test can pin the
/// executable-bit rule without mutating the process environment.
///
/// `which` requires the executable bit, which the previous `is_file` scan did
/// not: a non-executable match on PATH would have been returned and then
/// failed to spawn.
fn find_in_paths(binary: &str, paths: &std::ffi::OsStr) -> Option<PathBuf> {
    which::which_in(binary, Some(paths), ".").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_voicevox_url_accepts_loopback_and_rejects_remote() {
        assert_eq!(
            is_local_voicevox_url("http://127.0.0.1:50021"),
            Some(("127.0.0.1".into(), 50021))
        );
        assert_eq!(
            is_local_voicevox_url("http://localhost:50025"),
            Some(("127.0.0.1".into(), 50025))
        );
        assert_eq!(
            is_local_voicevox_url("http://127.0.0.1"),
            Some(("127.0.0.1".into(), 50021))
        );
        assert_eq!(is_local_voicevox_url("https://example.com:50021"), None);
        assert_eq!(is_local_voicevox_url("http://192.168.1.10:50021"), None);
    }

    #[test]
    fn find_engine_binary_reads_explicit_options_engine_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake_exe = dir.path().join("run.exe");
        std::fs::write(&fake_exe, b"stub").unwrap();
        let opts = serde_json::json!({
            "speaker": 3,
            "engine_path": fake_exe.to_string_lossy(),
        })
        .to_string();
        assert_eq!(
            find_engine_binary(&opts).as_deref(),
            Some(fake_exe.as_path())
        );
    }

    /// `find_on_path` requires the executable bit, so a plain file on PATH is
    /// not reported as a usable engine.
    #[cfg(unix)]
    #[test]
    fn find_on_path_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let name = "ene-voicevox-probe";
        let plain = dir.path().join(name);
        std::fs::write(&plain, b"stub").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let paths = dir.path().as_os_str();

        assert_eq!(
            find_in_paths(name, paths),
            None,
            "a non-executable file on PATH must not be chosen"
        );
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_in_paths(name, paths).as_deref(),
            Some(plain.as_path()),
            "the executable file on PATH must be found"
        );
    }
}
