use futures_util::FutureExt;
use rig_ecs::prelude::Wake;
use std::future::Future;
use tokio_util::task::AbortOnDropHandle;

pub(crate) struct Background<T>(AbortOnDropHandle<T>);

impl<T: Send + 'static> Background<T> {
    pub(crate) fn spawn(
        runtime: &tokio::runtime::Handle,
        wake: Option<&Wake>,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        let wake = wake.cloned();
        Self(AbortOnDropHandle::new(runtime.spawn(async move {
            let outcome = work.await;
            if let Some(wake) = wake {
                wake.wake();
            }
            outcome
        })))
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    pub(crate) fn take(self) -> Option<T> {
        self.0.now_or_never().and_then(Result::ok)
    }

    pub(crate) fn abort(&self) {
        self.0.abort();
    }
}
