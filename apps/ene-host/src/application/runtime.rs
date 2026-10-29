//! Accepted mutations belong to the application, never to an HTTP waiter.
use super::*;
use futures_util::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe, sync::Mutex as AdmissionMutex};
use tokio::sync::Semaphore;

pub(super) struct MutationExecutor {
    closed: AdmissionMutex<bool>,
    tasks: TaskTracker,
    slots: Arc<Semaphore>,
}

impl Default for MutationExecutor {
    fn default() -> Self {
        Self::new(MAX_PENDING_MUTATIONS)
    }
}
impl MutationExecutor {
    pub fn new(capacity: usize) -> Self {
        Self {
            closed: AdmissionMutex::new(false),
            tasks: TaskTracker::new(),
            slots: Arc::new(Semaphore::new(capacity)),
        }
    }
}

impl MutationExecutor {
    pub fn close(&self) {
        let mut closed = self.closed.lock().expect("admission lock");
        *closed = true;
        self.tasks.close();
    }

    pub async fn wait(&self) {
        self.tasks.wait().await;
    }

    pub async fn submit<T: Send + 'static>(
        &self,
        app: HostApplication,
        operation: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> Result<T, Error> {
        let job = {
            let closed = self.closed.lock().map_err(|_| Error::unavailable())?;
            if *closed || *app.0.shutdown.borrow() {
                return Err(Error::unavailable());
            }
            let slot = self.slots.clone().try_acquire_owned().map_err(|_| Error {
                kind: ErrorKind::StateConflict,
                reason: Some("busy"),
            })?;
            self.tasks.spawn(async move {
                let _slot = slot;
                match AssertUnwindSafe(operation).catch_unwind().await {
                    Ok(result) => {
                        if result
                            .as_ref()
                            .is_err_and(|error| error.kind == ErrorKind::StorageFailure)
                        {
                            let mut controls = app.0.controls.lock().await;
                            let mut boundary = app.0.boundary.lock().await;
                            controls.fail_storage(&app, &mut boundary);
                        }
                        result
                    }
                    Err(_) => {
                        let mut controls = app.0.controls.lock().await;
                        let mut boundary = app.0.boundary.lock().await;
                        controls.fail_storage(&app, &mut boundary);
                        Err(Error::new(ErrorKind::StorageFailure))
                    }
                }
            })
        };
        // Dropping this waiter never drops the admitted operation.
        job.await.map_err(|_| Error::unavailable())?
    }
}

impl HostApplication {
    async fn owned<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T, Error>> + Send + 'static,
    ) -> Result<T, Error> {
        self.0.mutations.submit(self.clone(), operation).await
    }

    pub async fn initialize(&self) -> Result<(), Error> {
        let app = self.clone();
        self.owned(async move { app.initialize_inner().await })
            .await
    }
    pub async fn patch_settings(&self, request: SettingsPatch) -> Result<Settings, Error> {
        self.owned(operations::patch_settings(self.clone(), request))
            .await
    }
    pub async fn patch_companion(
        &self,
        id: String,
        request: CompanionPatch,
    ) -> Result<Companion, Error> {
        CompanionId::parse(id.clone())?;
        self.owned(operations::patch_companion(self.clone(), id, request))
            .await
    }
    pub async fn post_grant(
        &self,
        key: String,
        request: GrantInput,
    ) -> Result<(crate::ports::Outcome, serde_json::Value), Error> {
        RequestKey::parse(key.clone())?;
        CompanionId::parse(request.companion_id.clone())?;
        ConnectionId::parse(request.connection_id.clone())?;
        self.owned(operations::post_grant(self.clone(), key, request))
            .await
    }
    pub async fn patch_grant(
        &self,
        id: String,
        request: GrantPatch,
    ) -> Result<ene_rpc::GrantResult, Error> {
        GrantId::parse(id.clone())?;
        self.owned(operations::patch_grant(self.clone(), id, request))
            .await
    }
    pub async fn delete_grant(&self, id: String) -> Result<(), Error> {
        GrantId::parse(id.clone())?;
        self.owned(operations::delete_grant(self.clone(), id)).await
    }
    pub async fn accept_message(
        &self,
        id: String,
        key: String,
        request: MessageInput,
    ) -> Result<(crate::ports::Outcome, serde_json::Value), Error> {
        CompanionId::parse(id.clone())?;
        RequestKey::parse(key.clone())?;
        self.owned(operations::accept_message(self.clone(), id, key, request))
            .await
    }
    pub async fn check(
        &self,
        id: String,
        key: String,
        request: CheckInput,
    ) -> Result<(crate::ports::Outcome, serde_json::Value), Error> {
        ConnectionId::parse(id.clone())?;
        CompanionId::parse(request.companion_id.clone())?;
        RequestKey::parse(key.clone())?;
        self.owned(operations::check(self.clone(), id, key, request))
            .await
    }
    pub async fn register_credentials(
        &self,
        id: String,
        value: Option<String>,
        source: Option<String>,
    ) -> Result<ene_rpc::CredentialResult, Error> {
        ConnectionId::parse(id.clone())?;
        let app = self.clone();
        self.owned(async move { app.register_credentials_inner(id, value, source).await })
            .await
    }
    pub async fn revoke_credentials(&self, id: String) -> Result<(), Error> {
        ConnectionId::parse(id.clone())?;
        let app = self.clone();
        self.owned(async move { app.revoke_credentials_inner(id).await })
            .await
    }
    pub async fn reload_providers(&self) -> Result<(), Error> {
        let app = self.clone();
        self.owned(async move { app.reload_providers_inner().await })
            .await
    }
    pub async fn enable_provider(&self, id: String, enabled: bool) -> Result<(), Error> {
        ProviderId::parse(id.clone())?;
        let app = self.clone();
        self.owned(async move { app.enable_provider_inner(id, enabled).await })
            .await
    }
}
