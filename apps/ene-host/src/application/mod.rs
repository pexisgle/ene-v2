mod commit;
mod credentials;
mod generation;
mod operations;
mod plugins;
mod runtime;
use crate::{domain::*, ports::Repository};
use crate::{llm::ProviderRegistry, ports::Credentials};
pub use ene_rpc::Notification;
use generation::{Controls, config};
pub use operations::{
    CheckInput, CompanionPatch, ConnectionInput, GrantInput, GrantPatch, MessageInput,
    SettingsPatch,
};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::task::TaskTracker;
use uuid::Uuid;
pub struct Subscription {
    pub id: String,
    pub cursor: Cursor,
    pub receiver: broadcast::Receiver<Notification>,
    // A dropped RPC body invalidates its subscription even when it cannot run
    // async cleanup. The boundary never owns the lease.
    pub lease: Arc<()>,
}
pub struct SnapshotView {
    pub grants: Vec<Grant>,
    pub stored: StoredSnapshot,
    pub cursor: Cursor,
    pub health: ene_rpc::HostHealth,
}
pub(super) struct Boundary {
    cursor: Cursor,
    connections: HashMap<String, Weak<()>>,
}
struct Inner {
    repository: Arc<dyn Repository>,
    providers: Arc<dyn ProviderRegistry>,
    credentials: Arc<dyn Credentials>,
    plugins: Option<Arc<dyn crate::ports::plugin::Plugins>>,
    plugin_events: Arc<tokio::sync::Semaphore>,
    plugin_tasks: TaskTracker,
    controls: Mutex<Controls>,
    boundary: Mutex<Boundary>,
    events: broadcast::Sender<Notification>,
    shutdown: watch::Sender<bool>,
    generations: TaskTracker,
    mutations: runtime::MutationExecutor,
    stops: runtime::MutationExecutor,
    cleanups: TaskTracker,
    cleanup: std::sync::Mutex<credentials::CleanupState>,
}
#[derive(Clone)]
pub struct HostApplication(Arc<Inner>);
impl HostApplication {
    pub fn new(
        repository: Arc<dyn Repository>,
        providers: Arc<dyn ProviderRegistry>,
        credentials: Arc<dyn Credentials>,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        Self::with_plugins(repository, providers, credentials, shutdown, None)
    }
    pub fn with_plugins(
        repository: Arc<dyn Repository>,
        providers: Arc<dyn ProviderRegistry>,
        credentials: Arc<dyn Credentials>,
        shutdown: watch::Sender<bool>,
        plugins: Option<Arc<dyn crate::ports::plugin::Plugins>>,
    ) -> Self {
        Self(Arc::new(Inner {
            repository,
            providers,
            credentials,
            plugins,
            plugin_events: Arc::new(tokio::sync::Semaphore::new(16)),
            plugin_tasks: TaskTracker::new(),
            controls: Mutex::new(Controls::default()),
            shutdown,
            generations: TaskTracker::new(),
            mutations: runtime::MutationExecutor::default(),
            stops: runtime::MutationExecutor::new(8),
            cleanups: TaskTracker::new(),
            cleanup: std::sync::Mutex::new(Default::default()),
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
    pub async fn stop(&self) -> Result<(), Error> {
        self.0.plugin_tasks.close();
        self.0.stops.close();
        self.0.mutations.close();
        self.0.shutdown.send_replace(true);
        {
            let controls = self.0.controls.lock().await;
            self.0.generations.close();
            for run in controls.running.values() {
                run.cancel.send_replace(true);
            }
        }
        self.0.stops.wait().await;
        self.0.mutations.wait().await;
        self.0.providers.shutdown().await;
        if let Some(plugins) = &self.0.plugins {
            plugins.shutdown().await;
        }
        self.0.plugin_tasks.wait().await;
        // Cancellation only signals generation. Its terminal database write
        // and persistence-failure handling must finish before the runtime exits.
        self.0.generations.wait().await;
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
            cursor: boundary.cursor.clone(),
            receiver,
            lease,
        }
    }
    pub async fn unsubscribe(&self, id: &str) {
        self.0.boundary.lock().await.connections.remove(id);
    }
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<SnapshotView, Error> {
        let controls = self.0.controls.lock().await;
        let health = ene_rpc::HostHealth {
            ready: !controls.failed() && !*self.0.shutdown.borrow(),
            recovery_required: controls.failed(),
            stopping: *self.0.shutdown.borrow(),
            credential_cleanup_pending: self.0.cleanup.lock().expect("cleanup lock").pending.len()
                as u64,
            ..Default::default()
        };
        let boundary = self.0.boundary.lock().await;
        if subscription.is_some_and(|id| {
            boundary
                .connections
                .get(id)
                .is_none_or(|lease| lease.strong_count() == 0)
        }) {
            return Err(Error::new(ErrorKind::ResyncRequired));
        }
        let mut stored = self.0.repository.snapshot().await?;
        let mut config = self.0.repository.configuration().await?;
        generation::refresh(self, &controls, &mut config);
        stored.companions = config.companions.into_values().collect();
        let grants = config.grants;
        Ok(SnapshotView {
            grants,
            stored,
            cursor: boundary.cursor.clone(),
            health,
        })
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        CompanionId::parse(id.to_owned())?;
        self.companions()
            .await?
            .into_iter()
            .find(|companion| companion.id == id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }
    /// Explicit user cancellation is independent of any response stream.
    pub async fn cancel_companion(&self, id: String) -> Result<(), Error> {
        CompanionId::parse(id.clone())?;
        let app = self.clone();
        self.0
            .stops
            .submit(self.clone(), async move {
                app.companion(&id).await?;
                let controls = app.0.controls.lock().await;
                for run in controls.running.values().filter(|run| run.companion == id) {
                    run.cancel.send_replace(true);
                }
                Ok(())
            })
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
        ConnectionId::parse(id.clone())?;
        CompanionId::parse(companion.clone())?;
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
            connection_fingerprint: ene_rpc::JsonDocument {
                json: policy
                    .as_ref()
                    .map(|policy| policy.fingerprint.clone())
                    .unwrap_or(serde_json::Value::Null)
                    .to_string(),
                ..Default::default()
            }
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
        self.companion(&companion).await?;
        if !(1..=200).contains(&limit)
            || cursor
                .as_ref()
                .is_some_and(|cursor| !valid_cursor(&companion, cursor))
        {
            return Err(Error::invalid());
        }
        self.0.repository.messages(companion, cursor, limit).await
    }
    pub async fn message(&self, companion: String, id: String) -> Result<Message, Error> {
        CompanionId::parse(companion.clone())?;
        MessageId::parse(id.clone())?;
        self.0
            .repository
            .message(companion, id)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }
}
fn valid_cursor(companion: &str, cursor: &str) -> bool {
    cursor.split_once(':').is_some_and(|(sequence, id)| {
        id == companion
            && sequence
                .parse::<u64>()
                .is_ok_and(|n| n > 0 && n <= i64::MAX as u64)
    })
}
