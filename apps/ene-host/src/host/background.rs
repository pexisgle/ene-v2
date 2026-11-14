//! A model call that runs on the host's Tokio runtime while the ECS thread
//! keeps working.
use rig_ecs::prelude::Wake;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

/// One call running off the ECS thread. The ECS side checks
/// [`Background::is_finished`] on a later frame and reads the outcome once
/// with [`Background::take`]; the call wakes the loop when it stores it.
///
/// Dropping the value aborts the Tokio task, so an interrupt drops the
/// in-flight request with it rather than leaving it to answer nobody.
pub(crate) struct Background<T> {
    task: tokio::task::JoinHandle<()>,
    outcome: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> Background<T> {
    /// Runs `work` on `runtime` and wakes `wake` once its outcome is stored.
    pub(crate) fn spawn(
        runtime: &tokio::runtime::Handle,
        wake: Option<&Wake>,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        let outcome = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&outcome);
        let wake = wake.cloned();
        let task = runtime.spawn(async move {
            let value = work.await;
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(value);
            }
            if let Some(wake) = wake {
                wake.wake();
            }
        });
        Self { task, outcome }
    }

    /// A call whose task and slot the caller already built, for tests.
    #[cfg(test)]
    pub(crate) fn new(task: tokio::task::JoinHandle<()>, outcome: Arc<Mutex<Option<T>>>) -> Self {
        Self { task, outcome }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// The outcome of a finished call, or `None` while it still runs.
    pub(crate) fn take(&self) -> Option<T> {
        self.outcome.lock().ok().and_then(|mut slot| slot.take())
    }

    /// Cancels the call; the request is dropped with it.
    pub(crate) fn abort(&self) {
        self.task.abort();
    }
}

impl<T> Drop for Background<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}
