//! Start the local service on an explicit connection request. Its lifetime is
//! independent of the client, including cancellation during startup.
use super::*;
use std::{
    io,
    os::unix::process::CommandExt,
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
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_default()
                    .with_file_name("ene-host")
            });
        Self::new(Command::new(executable))
    }

    fn new(mut command: Command) -> Self {
        // Do not share terminal input or the client's Ctrl-C process group.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0);
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
        if listening(path).await? {
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
            if listening(path).await? {
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

pub(super) async fn listening(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => {
            return Err("接続情報を確認できません。実行環境の権限を確認してください。".into());
        }
        Ok(_) => {}
    }
    // Keep the existing owner, permissions, symlink and endpoint checks before
    // probing. An unsafe descriptor must never trigger another host process.
    let connection = descriptor(path)?;
    let url = url::Url::parse(&connection.endpoint).map_err(|_| "接続先が不正です")?;
    let address = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, url.port().unwrap());
    match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(address)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(false),
        _ => Err("ホストの応答を確認できません。しばらく待ってから再試行してください。".into()),
    }
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
