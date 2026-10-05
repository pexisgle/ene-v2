use super::{Command, Http, channel, settings::ActionResult, settings::action, transport::Error};
use tokio::sync::oneshot;

struct Mutation {
    http: Http,
    companion: String,
    command: Command,
    result: oneshot::Sender<Result<ActionResult, Error>>,
}

// Keep admitted writes alive across subscription reconnects. Aborting an HTTP
// future does not cancel a host mutation, so cancellation cannot order revokes.
pub(super) struct MutationQueue(channel::UnboundedSender<Mutation>);
impl MutationQueue {
    pub fn new() -> Self {
        let (sender, mut receiver) = channel::unbounded_channel::<Mutation>();
        tokio::spawn(async move {
            while let Some(mutation) = receiver.recv().await {
                let result = async {
                    // Refresh revocation targets and current collections, but
                    // never replace the draft revisions carried by the command.
                    let snapshot = mutation.http.snapshot(None).await?;
                    action(
                        mutation.http,
                        snapshot,
                        mutation.companion,
                        mutation.command,
                    )
                    .await
                }
                .await;
                let _ = mutation.result.send(result);
            }
        });
        Self(sender)
    }

    pub fn submit(
        &self,
        http: Http,
        companion: String,
        command: Command,
    ) -> oneshot::Receiver<Result<ActionResult, Error>> {
        let (result, receiver) = oneshot::channel();
        let _ = self.0.send(Mutation {
            http,
            companion,
            command,
            result,
        });
        receiver
    }
}
