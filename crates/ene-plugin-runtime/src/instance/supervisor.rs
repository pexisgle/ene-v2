//! Bounded set of provider generation tasks. One JoinSet both waits for them
//! and aborts them, so there is one record of what is running.
use crate::{Error, Result, limits::MAX_INSTANCES};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinSet};

/// How long a clean shutdown waits before it aborts the remaining tasks.
const GRACE: Duration = Duration::from_secs(4);

#[derive(Clone, Default)]
pub struct Supervisor {
    tasks: Arc<Mutex<Tasks>>,
}

#[derive(Default)]
struct Tasks {
    running: JoinSet<()>,
    closed: bool,
}

impl Supervisor {
    pub async fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut tasks = self.tasks.lock().await;
        if tasks.closed {
            return Err(Error::Unavailable);
        }
        // Finished tasks are collected here, so the count is what is running.
        while tasks.running.try_join_next().is_some() {}
        if tasks.running.len() >= MAX_INSTANCES {
            return Err(Error::Unavailable);
        }
        tasks.running.spawn(task);
        Ok(())
    }

    pub async fn shutdown(&self) {
        // Closing under the lock `spawn` takes keeps a task from starting after
        // the last wait; the token set is awaited without holding it.
        let mut running = {
            let mut tasks = self.tasks.lock().await;
            tasks.closed = true;
            std::mem::take(&mut tasks.running)
        };
        let deadline = tokio::time::Instant::now() + GRACE;
        loop {
            match tokio::time::timeout_at(deadline, running.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    running.abort_all();
                    while running.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }
}
