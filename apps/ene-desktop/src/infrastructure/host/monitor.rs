//! Observe the service independently of the conversation subscription. Probing
//! never launches a process, opens a WebSocket, or contacts an AI provider.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    // 0 unknown, 1 stopped, 2 responding, 3 unable to verify.
    pub state: i32,
    pub endpoint: String,
    pub detail: String,
}
impl RuntimeStatus {
    pub(super) fn unavailable(detail: String) -> Self {
        Self {
            state: 3,
            endpoint: String::new(),
            detail,
        }
    }
}

async fn inspect(path: &Path) -> RuntimeStatus {
    match super::launch::listening(path).await {
        Ok(false) => RuntimeStatus {
            state: 1,
            endpoint: String::new(),
            detail: String::new(),
        },
        Err(reason) => RuntimeStatus::unavailable(reason),
        Ok(true) => {
            let result = async {
                let connection = descriptor(path)?;
                let http = Http::new(&connection).map_err(|_| "接続情報が不正です".to_owned())?;
                // A listening port alone does not establish service health or
                // authentication. Settings is a bounded, local read operation.
                http.settings().await.map_err(|_| {
                    "ホストに接続できません。接続情報やホストの状態を確認してください。".to_owned()
                })?;
                Ok::<_, String>(connection.endpoint)
            };
            match tokio::time::timeout(Duration::from_secs(2), result).await {
                Ok(Ok(endpoint)) => RuntimeStatus {
                    state: 2,
                    endpoint,
                    detail: String::new(),
                },
                Ok(Err(reason)) => RuntimeStatus::unavailable(reason),
                Err(_) => RuntimeStatus::unavailable(
                    "ホストの応答がありません。状態を再確認してください。".into(),
                ),
            }
        }
    }
}

pub(super) async fn run(
    mut commands: channel::UnboundedReceiver<Request>,
    emit: &(impl Fn(Event) + Sync),
) {
    let path = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(root) => root.join("ene-host/connection.json"),
        Err(_) => {
            emit(Event::Runtime(RuntimeStatus::unavailable(
                "実行環境を確認できません。デスクトップセッションで起動し直してください。".into(),
            )));
            return;
        }
    };
    loop {
        emit(Event::Runtime(inspect(&path).await));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {},
            command = commands.recv() => if command.is_none() { return; },
        }
    }
}
