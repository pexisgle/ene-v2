//! Cancellation has priority over timeout and completion for every outbound operation.
use super::{Operation, watch};
use std::future::Future;

pub(in crate::application) enum RunFailure<E> {
    Cancelled,
    TimedOut,
    Failed(E),
}

impl Operation {
    pub(in crate::application) async fn wait<T, E>(
        self,
        cancelled: &mut watch::Receiver<bool>,
        shutdown: &mut watch::Receiver<bool>,
        work: impl Future<Output = Result<T, E>>,
    ) -> Result<T, RunFailure<E>> {
        if *cancelled.borrow() || *shutdown.borrow() {
            return Err(RunFailure::Cancelled);
        }
        let result = tokio::select! {
            biased;
            _ = cancelled.changed() => Err(RunFailure::Cancelled),
            _ = shutdown.changed() => Err(RunFailure::Cancelled),
            result = tokio::time::timeout(self.timeout(), work) => match result {
                Ok(result) => result.map_err(RunFailure::Failed),
                Err(_) => Err(RunFailure::TimedOut),
            },
        };
        if *cancelled.borrow() || *shutdown.borrow() {
            Err(RunFailure::Cancelled)
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_wins_when_completion_is_already_ready() {
        let (cancel, mut cancelled) = watch::channel(false);
        let (_stop, mut shutdown) = watch::channel(false);
        let result = Operation::ModelList
            .wait(&mut cancelled, &mut shutdown, async {
                cancel.send_replace(true);
                Ok::<_, ()>("late response")
            })
            .await;
        assert!(matches!(result, Err(RunFailure::Cancelled)));
    }
}
