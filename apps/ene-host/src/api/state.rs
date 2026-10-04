use ene_core::application::HostApplication;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
#[derive(Clone)]
pub struct App {
    pub application: HostApplication,
    pub(crate) token: Arc<str>,
    pub shutdown: watch::Sender<bool>,
    mutations: TaskTracker,
}
impl App {
    pub fn new(application: HostApplication, token: String, shutdown: watch::Sender<bool>) -> Self {
        Self {
            application,
            token: token.into(),
            shutdown,
            mutations: TaskTracker::new(),
        }
    }

    /// Once admitted, finish SQL, authority updates and notifications even if HTTP disconnects.
    pub async fn mutate<T: Send + 'static>(
        &self,
        operation: impl std::future::Future<Output = ene_core::ports::Result<T>> + Send + 'static,
    ) -> Result<T, super::error::ApiError> {
        if *self.shutdown.borrow() || self.mutations.is_closed() {
            return Err(super::error::ApiError::unavailable());
        }
        Ok(self
            .mutations
            .spawn(operation)
            .await
            .map_err(|_| super::error::ApiError::unavailable())??)
    }

    pub async fn finish_mutations(&self) {
        self.mutations.close();
        self.mutations.wait().await;
    }
}
