//! Bounded tracker for provider generation tasks.
use crate::{Error, Result, limits::MAX_INSTANCES};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Default)]
pub struct Supervisor {
    tracker: tokio_util::task::TaskTracker,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}
impl Supervisor {
    pub fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut tasks = self.tasks.lock().map_err(|_| Error::Unavailable)?;
        if self.tracker.is_closed() {
            return Err(Error::Unavailable);
        }
        tasks.retain(|t| !t.is_finished());
        if tasks.len() >= MAX_INSTANCES {
            return Err(Error::Unavailable);
        }
        tasks.push(self.tracker.spawn(task).abort_handle());
        Ok(())
    }
    pub async fn shutdown(&self) {
        {
            // `TaskTracker::close` only releases `wait`; it does not refuse a
            // later `spawn`. Closing under the lock `spawn` takes keeps a task
            // from slipping in after the last `wait` check.
            let gate = self.tasks.lock();
            self.tracker.close();
            drop(gate);
        }
        if tokio::time::timeout(Duration::from_secs(4), self.tracker.wait())
            .await
            .is_err()
        {
            if let Ok(tasks) = self.tasks.lock() {
                for task in tasks.iter() {
                    task.abort();
                }
            }
            self.tracker.wait().await;
        }
    }
}
