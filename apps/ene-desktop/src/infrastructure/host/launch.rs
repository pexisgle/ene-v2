//! Start the local service on an explicit connection request. Its lifetime is
//! independent of the client, including cancellation during startup.
use super::*;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    io,
    path::PathBuf,
    process::{Child, Command, Stdio},
};
use tokio::{net::TcpStream, time::Instant};

pub(super) struct Launcher {
    command: Command,
    child: Option<Child>,
}

impl Launcher {
    pub(super) fn local() -> Self {
        let executable = std::env::var_os("ENE_HOST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| ene_platform::sibling_executable("ene-host"));
        Self::new(Command::new(executable))
    }

    fn new(mut command: Command) -> Self {
        // Do not share terminal input or the client's Ctrl-C process group.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x00000200 | 0x08000000);
        Self {
            command,
            child: None,
        }
    }

    pub(super) async fn prepare(
        &mut self,
        path: &Path,
        emit: &(impl Fn(Event) + Sync),
    ) -> Result<(), String> {
        if responsive(path).await? {
            return Ok(());
        }
        let running = match self.child.as_mut() {
            Some(child) => child
                .try_wait()
                .map_err(|_| "ホストの起動状態を確認できません")?
                .is_none(),
            None => false,
        };
        if !running {
            self.child = Some(self.command.spawn().map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => "ホストの実行ファイルが見つかりません。ene-hostをデスクトップアプリと同じフォルダーに配置して、再試行してください。",
                io::ErrorKind::PermissionDenied => "ホストを起動する権限がありません。実行ファイルの権限を確認して、再試行してください。",
                _ => "ホストを起動できません。実行環境を確認して、再試行してください。",
            })?);
        }
        emit(Event::Connection(
            ConnectionPhase::Starting,
            "このPCのホストを起動しています…".into(),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Another client may have won the host's singleton lock.
            if responsive(path).await? {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .map_err(|_| "ホストの起動状態を確認できません")?
            {
                return Err(format!(
                    "ホストが起動中に終了しました（{status}）。実行環境や保存先の権限を確認して、再試行してください。"
                ));
            }
            if Instant::now() >= deadline {
                return Err(
                    "ホストの起動を確認できませんでした。しばらく待ってから再試行してください。"
                        .into(),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub(super) enum Probe {
    Stopped,
    Ready(RuntimeConnection),
    Unverified(String),
}

async fn responsive(path: &Path) -> Result<bool, String> {
    Ok(matches!(probe(path).await?, Probe::Ready(_)))
}

pub(super) async fn probe(path: &Path) -> Result<Probe, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Probe::Stopped),
        Err(_) => {
            return Err("接続情報を確認できません。実行環境の権限を確認してください。".into());
        }
        Ok(_) => {}
    }
    // Keep the existing owner, permissions, symlink and endpoint checks before
    // probing. An unsafe descriptor must never trigger another host process.
    let connection = descriptor(path)?;
    let http = Http::new(&connection).map_err(|_| "接続情報が不正です")?;
    let url = url::Url::parse(&connection.endpoint).map_err(|_| "接続先が不正です")?;
    let address = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, url.port().unwrap());
    match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(address)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
            return Ok(Probe::Stopped);
        }
        _ => {
            return Ok(Probe::Unverified(
                "ホストの応答を確認できません。状態を再確認してください。".into(),
            ));
        }
    }
    // A reused port, a rejected stale token, and an unrelated HTTP server do
    // not identify this host. Only an authenticated, typed API response does.
    Ok(
        match tokio::time::timeout(Duration::from_secs(1), http.settings()).await {
            Ok(Ok(_)) => Probe::Ready(connection),
            _ => Probe::Unverified(
                "ホストに接続できません。接続情報やホストの状態を確認してください。".into(),
            ),
        },
    )
}

impl Drop for Launcher {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Reap while the desktop is alive; exiting the desktop leaves the
            // service running and transfers reaping to the OS.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn stale_descriptor_with_a_reused_port_still_attempts_startup() {
        for response in [
            axum::http::StatusCode::UNAUTHORIZED,
            axum::http::StatusCode::NOT_FOUND,
            axum::http::StatusCode::OK,
        ] {
            let router = axum::Router::new().route(
                "/rpc/ene.host.v1.HostService/GetSettings",
                axum::routing::post(
                    move || async move { (response, "not an ene settings response") },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("connection.json");
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "endpoint": format!("http://{}/rpc", listener.local_addr().unwrap()),
                    "token": "stale-token",
                }))
                .unwrap(),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, super::super::connect_fixture::router(router))
                    .await
                    .unwrap()
            });
            let mut launcher = Launcher::new(Command::new(temp.path().join("missing-host")));
            assert!(matches!(probe(&path).await.unwrap(), Probe::Unverified(_)));
            assert!(
                launcher
                    .prepare(&path, &|_| {})
                    .await
                    .unwrap_err()
                    .contains("実行ファイルが見つかりません")
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn authenticated_host_is_reused_and_unsafe_descriptor_never_launches() {
        let router = axum::Router::new().route(
            "/rpc/ene.host.v1.HostService/GetSettings",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-token");
                axum::Json(ene_rpc::Settings {
                    revision: 1,
                    connections: vec![],
                    ..Default::default()
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connection.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "endpoint": format!("http://{}/rpc", listener.local_addr().unwrap()),
                "token": "fixture-token",
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::super::connect_fixture::router(router))
                .await
                .unwrap()
        });
        let mut launcher = Launcher::new(Command::new(temp.path().join("missing-host")));
        launcher.prepare(&path, &|_| {}).await.unwrap();
        assert!(launcher.child.is_none());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            launcher
                .prepare(&path, &|_| {})
                .await
                .unwrap_err()
                .contains("権限")
        );
        assert!(launcher.child.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn a_port_that_never_answers_does_not_block_startup_indefinitely() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connection.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "endpoint": format!("http://{}/rpc", listener.local_addr().unwrap()),
                "token": "stale",
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut launcher = Launcher::new(Command::new(temp.path().join("missing-host")));
        let result = tokio::time::timeout(Duration::from_secs(3), launcher.prepare(&path, &|_| {}))
            .await
            .unwrap();
        assert!(result.unwrap_err().contains("実行ファイルが見つかりません"));
    }
}
