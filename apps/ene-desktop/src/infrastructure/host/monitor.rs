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
    use super::launch::Probe;
    match super::launch::probe(path).await {
        Ok(Probe::Stopped) => RuntimeStatus {
            state: 1,
            endpoint: String::new(),
            detail: String::new(),
        },
        Err(reason) | Ok(Probe::Unverified(reason)) => RuntimeStatus::unavailable(reason),
        Ok(Probe::Ready(connection)) => RuntimeStatus {
            state: 2,
            endpoint: connection.endpoint,
            detail: String::new(),
        },
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
