//! Local VOICEVOX Engine process discovery, lifecycle management and readiness probing.

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{
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

/// Owns at most one locally spawned VOICEVOX Engine child process.
#[derive(Clone, Default)]
pub(crate) struct VoicevoxManager {
    inner: Arc<Mutex<ManagedProcess>>,
}

impl VoicevoxManager {
    /// Ensures a local VOICEVOX Engine at `base_url` is reachable, launching it if needed.
    pub(crate) async fn ensure_running(
        &self,
        base_url: &str,
        options_json: &str,
    ) -> Result<(), String> {
        let base = base_url.trim().trim_end_matches('/');
        let base = if base.is_empty() {
            "http://127.0.0.1:50021"
        } else {
            base
        };

        if cfg!(test) && std::env::var_os("ENE_VOICEVOX_ENGINE").is_none() {
            return Err("VOICEVOX自動起動はテスト実行中はスキップされます".into());
        }

        let mut guard = self.inner.lock().await;
        if is_reachable(base, Duration::from_millis(1_200)).await {
            return Ok(());
        }
        let Some((host, port)) = is_local_voicevox_url(base) else {
            return Err("リモートのVOICEVOX接続先は自動起動できません".into());
        };

        let same_running = if guard.endpoint == base {
            if let Some(child) = guard.child.as_mut() {
                matches!(child.try_wait(), Ok(None))
            } else {
                false
            }
        } else {
            false
        };

        if !same_running {
            stop_inner(&mut guard).await;
            let exe = find_engine_binary(options_json).ok_or_else(|| {
                "VOICEVOXエンジンが見つかりません。VOICEVOXをインストールするか、環境変数 ENE_VOICEVOX_ENGINE に run.exe のパスを設定してください。".to_owned()
            })?;
            tracing::info!(engine = %exe.display(), host = %host, port, "starting local VOICEVOX engine");
            let child = spawn_engine(&exe, &host, port)?;
            guard.endpoint = base.to_owned();
            guard.child = Some(child);
        }

        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if is_reachable(base, Duration::from_millis(1_000)).await {
                tracing::info!(endpoint = %base, "VOICEVOX engine is ready");
                return Ok(());
            }
            if let Some(child) = guard.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                guard.child = None;
                return Err(format!(
                    "VOICEVOXエンジンが起動直後に終了しました（code: {status}）"
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Err("VOICEVOXエンジンの起動待機がタイムアウトしました（25秒）".into())
    }

    /// Stops any VOICEVOX Engine child process managed by this host.
    pub(crate) async fn stop(&self) {
        let mut guard = self.inner.lock().await;
        stop_inner(&mut guard).await;
    }
}

async fn stop_inner(managed: &mut ManagedProcess) {
    if let Some(mut child) = managed.child.take() {
        let _ = Box::into_pin(child.kill()).await;
        let _ = child.wait().await;
    }
    managed.endpoint.clear();
}

fn spawn_engine(exe: &Path, host: &str, port: u16) -> Result<Box<dyn ChildWrapper>, String> {
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
    wrap.wrap(KillOnDrop).spawn().map_err(|error| {
        format!(
            "VOICEVOXエンジンの起動に失敗しました ({}): {error}",
            exe.display()
        )
    })
}

pub(crate) async fn is_reachable(base_url: &str, timeout: Duration) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return false;
    };
    let url = format!("{}/speakers", base_url.trim_end_matches('/'));
    client
        .get(url)
        .send()
        .await
        .is_ok_and(|res| res.status().is_success())
}

/// Parses a loopback HTTP URL for VOICEVOX (`http://127.0.0.1:50021`, `http://localhost:50021`, etc.).
pub(crate) fn is_local_voicevox_url(base: &str) -> Option<(String, u16)> {
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
pub(crate) fn find_engine_binary(options_json: &str) -> Option<PathBuf> {
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

    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
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
        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
            return Some(found);
        }
        if let Some(found) = find_on_path("voicevox_engine.exe") {
            return Some(found);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let mut candidates = vec![PathBuf::from(
            "/Applications/VOICEVOX.app/Contents/Resources/vv-engine/run",
        )];
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            candidates.push(
                home.join("Applications")
                    .join("VOICEVOX.app")
                    .join("Contents")
                    .join("Resources")
                    .join("vv-engine")
                    .join("run"),
            );
        }
        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
            return Some(found);
        }
        if let Some(found) = find_on_path("voicevox_engine") {
            return Some(found);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut candidates = vec![
            PathBuf::from("/opt/voicevox/vv-engine/run"),
            PathBuf::from("/usr/local/bin/voicevox_engine"),
        ];
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            candidates.push(
                home.join(".local")
                    .join("share")
                    .join("voicevox")
                    .join("vv-engine")
                    .join("run"),
            );
        }
        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
            return Some(found);
        }
        if let Some(found) = find_on_path("voicevox_engine") {
            return Some(found);
        }
    }

    None
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
}
