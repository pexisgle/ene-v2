use super::{
    Admission, Command, Http, channel, settings::ActionResult, settings::action_with_key,
    transport::Error,
};
use ene_api_client::IdempotencyKey;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

struct Mutation {
    http: Http,
    companion: String,
    command: Command,
    key: IdempotencyKey,
    host: String,
    result: oneshot::Sender<Result<ActionResult, Error>>,
    _permit: OwnedSemaphorePermit,
}

// This owner outlives each subscription. Writes remain ordered; long running
// checks have a separate lane so revocation never waits for a provider response.
pub(super) struct MutationQueue {
    sender: channel::Sender<Mutation>,
    slots: Arc<Semaphore>,
}
impl MutationQueue {
    pub fn new() -> Self {
        let (sender, mut receiver) = channel::channel::<Mutation>(64);
        tokio::spawn(async move {
            let mut checks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    mutation = receiver.recv() => {
                        let Some(mutation) = mutation else { break; };
                        if mutation.command.policy().admission == Admission::DurableExecution {
                            checks.spawn(execute(mutation));
                        } else { execute(mutation).await; }
                    },
                    _ = checks.join_next(), if !checks.is_empty() => {},
                }
            }
            while checks.join_next().await.is_some() {}
        });
        Self {
            sender,
            slots: Arc::new(Semaphore::new(64)),
        }
    }

    pub fn submit(
        &self,
        http: Http,
        companion: String,
        command: Command,
        key: IdempotencyKey,
        host: String,
    ) -> oneshot::Receiver<Result<ActionResult, Error>> {
        let (result, receiver) = oneshot::channel();
        let Ok(permit) = self.slots.clone().try_acquire_owned() else {
            let _ = result.send(Err(Error::Api(
                "要求が多すぎます。処理の完了後に再試行してください。".into(),
            )));
            return receiver;
        };
        if let Err(error) = self.sender.try_send(Mutation {
            http,
            companion,
            command,
            key,
            host,
            result,
            _permit: permit,
        }) {
            let _ = error.into_inner().result.send(Err(Error::Api(
                "接続処理が停止しました。再接続してください。".into(),
            )));
        }
        receiver
    }
}
async fn execute(mutation: Mutation) {
    let result = async {
        let snapshot = mutation.http.snapshot(None).await?;
        if snapshot.host_id != mutation.host {
            return Err(Error::Api(
                "接続先ホストが変わったため操作を停止しました。設定を確認して再実行してください。"
                    .into(),
            ));
        }
        action_with_key(
            mutation.http,
            snapshot,
            mutation.companion,
            mutation.command,
            mutation.key,
        )
        .await
    }
    .await;
    let _ = mutation.result.send(result);
}
pub(super) fn is_mutation(command: &Command) -> bool {
    matches!(
        command.policy().admission,
        Admission::Write | Admission::DurableExecution
    )
}
