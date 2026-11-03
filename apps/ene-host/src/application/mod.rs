mod commit;
mod conversation;
mod credentials;
mod generation;
mod inference;
pub(crate) mod memory;
pub(crate) mod mind;
mod operations;
mod plugins;
mod runtime;
use crate::domain::*;
use crate::infrastructure::plugins::Manager;
use crate::infrastructure::sqlite::Store;
use crate::{llm::ProviderRegistry, ports::Credentials};
#[cfg(test)]
pub(crate) use ene_rpc::ConnectionInput;
pub use ene_rpc::Notification;
use generation::{Controls, config};
pub use operations::{
    CheckInput, CompanionPatch, GrantInput, GrantPatch, MessageInput, SettingsPatch,
};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::task::TaskTracker;
use uuid::Uuid;
#[cfg(test)]
pub(crate) type ConsolidationHook = std::sync::Arc<
    dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<String, Error>> + Send + Sync,
>;

pub struct Subscription {
    pub id: String,
    pub receiver: broadcast::Receiver<Notification>,
    // A dropped RPC body invalidates its subscription even when it cannot run
    // async cleanup. The boundary never owns the lease.
    pub lease: Arc<()>,
}
pub struct SnapshotView {
    pub grants: Vec<Grant>,
    pub stored: StoredSnapshot,
    pub messages: Vec<Message>,
    pub message_cursors: HashMap<String, Option<String>>,
    pub cursor: Cursor,
    pub health: ene_rpc::HostHealth,
}
pub(super) struct Boundary {
    cursor: Cursor,
    connections: HashMap<String, Weak<()>>,
}
struct Inner {
    repository: Arc<Store>,
    providers: Arc<dyn ProviderRegistry>,
    credentials: Arc<dyn Credentials>,
    plugins: Option<Arc<Manager>>,
    plugin_events: Arc<tokio::sync::Semaphore>,
    plugin_tasks: TaskTracker,
    background: TaskTracker,
    controls: Mutex<Controls>,
    boundary: Mutex<Boundary>,
    events: broadcast::Sender<Notification>,
    shutdown: watch::Sender<bool>,
    rig: std::sync::OnceLock<Arc<crate::rig::Runtime>>,
    #[cfg(test)]
    test_model: std::sync::OnceLock<Arc<crate::application::conversation::TestModel>>,
    mutations: runtime::MutationExecutor,
    stops: runtime::MutationExecutor,
    cleanups: TaskTracker,
    cleanup: std::sync::Mutex<credentials::CleanupState>,
    mind_notify: tokio::sync::Notify,
    mind_started: std::sync::atomic::AtomicBool,
    active_recall:
        std::sync::Mutex<std::collections::HashMap<String, crate::application::mind::ActiveRecall>>,
    prompt_bridge: std::sync::OnceLock<std::sync::Arc<crate::rig::prompt::PromptState>>,
    active_request: std::sync::Mutex<std::collections::HashMap<String, String>>,
    #[cfg(test)]
    consolidation_hook: std::sync::OnceLock<ConsolidationHook>,
}
#[derive(Clone)]
pub struct HostApplication(Arc<Inner>);
impl HostApplication {
    pub fn with_plugins(
        repository: Arc<Store>,
        providers: Arc<dyn ProviderRegistry>,
        credentials: Arc<dyn Credentials>,
        shutdown: watch::Sender<bool>,
        plugins: Option<Arc<Manager>>,
    ) -> Self {
        Self(Arc::new(Inner {
            repository,
            providers,
            credentials,
            plugins,
            plugin_events: Arc::new(tokio::sync::Semaphore::new(16)),
            plugin_tasks: TaskTracker::new(),
            background: TaskTracker::new(),
            controls: Mutex::new(Controls::default()),
            shutdown,
            rig: std::sync::OnceLock::new(),
            #[cfg(test)]
            test_model: std::sync::OnceLock::new(),
            mutations: runtime::MutationExecutor::default(),
            stops: runtime::MutationExecutor::new(8),
            cleanups: TaskTracker::new(),
            cleanup: std::sync::Mutex::new(Default::default()),
            mind_notify: tokio::sync::Notify::new(),
            mind_started: std::sync::atomic::AtomicBool::new(false),
            active_recall: std::sync::Mutex::new(std::collections::HashMap::new()),
            prompt_bridge: std::sync::OnceLock::new(),
            active_request: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(test)]
            consolidation_hook: std::sync::OnceLock::new(),
            events: broadcast::channel(256).0,
            boundary: Mutex::new(Boundary {
                cursor: Cursor {
                    epoch: Uuid::new_v4().to_string(),
                    seq: 0,
                    ..Default::default()
                },
                connections: HashMap::new(),
            }),
        }))
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_consolidation_hook(&self, hook: ConsolidationHook) {
        let _ = self.0.consolidation_hook.set(hook);
    }

    pub(crate) fn begin_shutdown(&self) {
        self.0.plugin_tasks.close();
        self.0.background.close();
        self.0.stops.close();
        self.0.mutations.close();
        self.0.shutdown.send_replace(true);
        if let Some(plugins) = &self.0.plugins {
            plugins.cancel();
        }
    }
    pub async fn stop(&self) -> Result<(), Error> {
        self.begin_shutdown();
        {
            let controls = self.0.controls.lock().await;
            for run in controls.running.values() {
                run.cancel.send_replace(true);
            }
        }
        self.0.stops.wait().await;
        self.0.mutations.wait().await;
        // The kernel stops its turns, writes the session log and exits
        // before the host reports a clean stop. Its failure never skips
        // the other components' shutdown, which could leave work running.
        let rig_shutdown = match self.0.rig.get() {
            Some(rig) => rig.shutdown().await,
            None => Ok(()),
        };
        let plugin_shutdown = match &self.0.plugins {
            Some(plugins) => plugins.shutdown().await,
            None => Ok(()),
        };
        self.0.plugin_tasks.wait().await;
        self.0.background.wait().await;
        self.0.cleanups.close();
        self.0.cleanups.wait().await;
        // One bounded second pass also retries any earlier observable failure.
        let pending: Vec<_> = self
            .0
            .cleanup
            .lock()
            .expect("cleanup lock")
            .pending
            .keys()
            .cloned()
            .collect();
        for reference in pending {
            self.schedule_forget(reference);
        }
        self.0.cleanups.close();
        self.0.cleanups.wait().await;
        rig_shutdown?;
        plugin_shutdown?;
        if self.0.controls.lock().await.failed() {
            return Err(Error::new(ErrorKind::StorageFailure));
        }
        if !self
            .0
            .cleanup
            .lock()
            .expect("cleanup lock")
            .pending
            .is_empty()
        {
            return Err(Error {
                kind: ErrorKind::Unavailable,
                reason: Some("credential_cleanup_failed"),
            });
        }
        Ok(())
    }
    pub fn shutdown(&self) -> watch::Receiver<bool> {
        self.0.shutdown.subscribe()
    }
    pub async fn subscribe(&self) -> Subscription {
        let mut boundary = self.0.boundary.lock().await;
        let id = Uuid::new_v4().to_string();
        let receiver = self.0.events.subscribe();
        boundary
            .connections
            .retain(|_, lease| lease.strong_count() > 0);
        let lease = Arc::new(());
        boundary
            .connections
            .insert(id.clone(), Arc::downgrade(&lease));
        Subscription {
            id,
            receiver,
            lease,
        }
    }
    pub async fn unsubscribe(&self, id: &str) {
        self.0.boundary.lock().await.connections.remove(id);
    }
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<SnapshotView, Error> {
        let (mut health, stored, grants) = {
            let controls = self.0.controls.lock().await;
            let health = ene_rpc::HostHealth {
                ready: !controls.failed() && !*self.0.shutdown.borrow(),
                recovery_required: controls.failed(),
                stopping: *self.0.shutdown.borrow(),
                credential_cleanup_pending: self
                    .0
                    .cleanup
                    .lock()
                    .expect("cleanup lock")
                    .pending
                    .len() as u64,
                ..Default::default()
            };
            let stored = self.0.repository.snapshot().await?;
            let mut config = self.0.repository.configuration().await?;
            generation::refresh(self, &controls, &mut config);
            let mut stored = stored;
            stored.companions = config.companions.into_values().collect();
            (health, stored, config.grants)
        };
        let cursor = {
            let boundary = self.0.boundary.lock().await;
            if subscription.is_some_and(|id| {
                boundary
                    .connections
                    .get(id)
                    .is_none_or(|lease| lease.strong_count() == 0)
            }) {
                return Err(Error::new(ErrorKind::ResyncRequired));
            }
            boundary.cursor.clone()
        };
        let mut messages = Vec::new();
        let mut message_cursors = HashMap::new();
        if let Ok(rig) = self.rig() {
            for companion in &stored.companions {
                match rig.history(companion.id.clone(), None, 50).await {
                    Ok(page) => {
                        message_cursors.insert(companion.id.clone(), page.next_cursor);
                        messages.extend(page.items);
                    }
                    Err(_) => {
                        // The snapshot still carries configuration and why the
                        // history is unavailable; it does not claim readiness
                        // or a complete history it could not read.
                        health.ready = false;
                        health.recovery_required = true;
                        message_cursors.insert(companion.id.clone(), None);
                    }
                }
            }
        }
        Ok(SnapshotView {
            grants,
            stored,
            messages,
            message_cursors,
            cursor,
            health,
        })
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        validate_id(id)?;
        self.companions()
            .await?
            .into_iter()
            .find(|companion| companion.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }
    /// Explicit user cancellation is independent of any response stream.
    pub async fn cancel_companion(&self, id: String) -> Result<(), Error> {
        validate_id(&id)?;
        let app = self.clone();
        self.0
            .stops
            .submit(
                self.clone(),
                async move { conversation::cancel(app, id).await },
            )
            .await
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        let mut controls = self.0.controls.lock().await;
        let mut configuration = config(self, &mut controls).await?;
        generation::refresh(self, &controls, &mut configuration);
        Ok(configuration.companions.into_values().collect())
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        Ok(self.0.repository.configuration().await?.settings)
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<Grant>, Error> {
        if let Some(id) = companion {
            self.companion(id).await?;
        }
        let mut controls = self.0.controls.lock().await;
        Ok(config(self, &mut controls)
            .await?
            .grants
            .into_iter()
            .filter(|g| companion.is_none_or(|id| g.companion_id == id))
            .collect())
    }
    pub async fn availability(
        &self,
        id: String,
        companion: String,
    ) -> Result<ene_rpc::Availability, Error> {
        validate_id(&id)?;
        validate_id(&companion)?;
        let mut controls = self.0.controls.lock().await;
        let configuration = config(self, &mut controls).await?;
        if !configuration.companions.contains_key(&companion) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        let connection = rules::connection(&configuration, &id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound))?;
        let policy = self
            .0
            .providers
            .resolve(&connection)
            .ok()
            .map(|binding| binding.policy().clone());
        let state = rules::ExecutionState {
            credential_present: controls.secrets.contains_key(&id),
            healthy: !controls.failed(),
            stopping: *self.0.shutdown.borrow(),
            busy: controls.running.len() >= MAX_EXECUTIONS,
        };
        Ok(ene_rpc::Availability {
            companion_id: companion.clone(),
            connection_id: id,
            settings_revision: configuration.settings.revision,
            provider_revision: policy.as_ref().map(|policy| policy.revision.clone()),
            destinations: policy
                .as_ref()
                .map(|policy| policy.destinations.clone())
                .unwrap_or_default(),
            connection_fingerprint: ene_rpc::JsonDocument::from(
                policy
                    .as_ref()
                    .map(|policy| policy.fingerprint.clone())
                    .unwrap_or(serde_json::Value::Null),
            )
            .into(),
            operations: OperationKind::ALL
                .into_iter()
                .map(|operation| {
                    rules::evaluate(
                        &configuration,
                        &companion,
                        &connection,
                        operation,
                        policy.as_ref(),
                        state,
                    )
                })
                .collect(),
            ..Default::default()
        })
    }
    pub async fn messages(
        &self,
        companion: String,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<ene_rpc::History, Error> {
        conversation::history(self, companion, cursor, limit).await
    }
    pub async fn message(&self, companion: String, id: String) -> Result<Message, Error> {
        conversation::message(self, companion, id).await
    }
}
