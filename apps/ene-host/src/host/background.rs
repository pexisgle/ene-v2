use futures_util::FutureExt;
use rig_ecs::prelude::Wake;
use std::future::Future;
pub(crate) use tokio_util::task::AbortOnDropHandle;

pub(crate) fn spawn<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    wake: Option<&Wake>,
    work: impl Future<Output = T> + Send + 'static,
) -> AbortOnDropHandle<T> {
    let wake = wake.cloned();
    AbortOnDropHandle::new(runtime.spawn(async move {
        let outcome = work.await;
        if let Some(wake) = wake {
            wake.wake();
        }
        outcome
    }))
}

pub(crate) fn take<T>(task: AbortOnDropHandle<T>) -> Option<T> {
    task.now_or_never().and_then(Result::ok)
}
