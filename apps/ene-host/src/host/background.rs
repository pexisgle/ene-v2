use futures_util::FutureExt;
use rig_ecs::prelude::Wake;
use std::{future::Future, panic::AssertUnwindSafe, task::Poll};
use tokio::sync::oneshot::{self, error::TryRecvError};
pub(crate) use tokio_util::task::AbortOnDropHandle;

/// One task whose result or closure is observable before its actor wakes.
pub(crate) struct Job<T> {
    result: oneshot::Receiver<T>,
    _task: AbortOnDropHandle<()>,
}

impl<T> Job<T> {
    pub(crate) fn poll(&mut self) -> Poll<Option<T>> {
        match self.result.try_recv() {
            Ok(value) => Poll::Ready(Some(value)),
            Err(TryRecvError::Closed) => Poll::Ready(None),
            Err(TryRecvError::Empty) => Poll::Pending,
        }
    }

    pub(crate) async fn wait(self) -> Option<T> {
        self.result.await.ok()
    }
}

pub(crate) fn spawn<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    wake: Option<&Wake>,
    work: impl Future<Output = T> + Send + 'static,
) -> Job<T> {
    let wake = wake.cloned();
    let (publish, result) = oneshot::channel();
    let task = runtime.spawn(async move {
        match AssertUnwindSafe(work).catch_unwind().await {
            Ok(outcome) => {
                let _published = publish.send(outcome);
            }
            Err(_) => drop(publish),
        }
        if let Some(wake) = wake {
            wake.wake();
        }
    });
    Job {
        result,
        _task: AbortOnDropHandle::new(task),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Mutex, mpsc},
        time::Duration,
    };

    #[test]
    fn the_result_is_available_at_the_wake_boundary() {
        let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
        let (woke, on_wake) = mpsc::channel();
        let (finish, waiting) = mpsc::channel();
        let waiting = Mutex::new(waiting);
        let timeout = Duration::from_secs(5);
        let wake = Wake::new(move || {
            woke.send(()).unwrap();
            waiting.lock().unwrap().recv_timeout(timeout).unwrap();
        });
        let mut job = spawn(runtime.handle(), Some(&wake), async { 42 });
        on_wake.recv_timeout(timeout).unwrap();
        let result = job.poll();
        finish.send(()).unwrap();
        assert_eq!(result, Poll::Ready(Some(42)));
    }
}
