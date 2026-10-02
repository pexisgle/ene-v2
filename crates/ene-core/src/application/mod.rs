mod credentials;
mod generation;
pub mod operations;
use crate::ports::{Credentials, llm::ProviderRegistry};
use crate::{domain::*, ports::Repository};
use generation::{Controls, config};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::{Mutex, broadcast, watch};
use uuid::Uuid;
#[derive(Clone, Debug)]
pub struct Notification {
    pub epoch: String,
    pub seq: u64,
    pub kind: String,
    pub companion_id: Option<String>,
    pub data: serde_json::Value,
}
pub struct Subscription {
    pub id: String,
    pub cursor: Cursor,
    pub receiver: broadcast::Receiver<Notification>,
}
pub struct SnapshotView {
    pub grants: Vec<Grant>,
    pub stored: StoredSnapshot,
    pub cursor: Cursor,
}
pub(super) struct Boundary {
    cursor: Cursor,
    connections: HashSet<String>,
}
struct Inner {
    repository: Arc<dyn Repository>,
    providers: Arc<dyn ProviderRegistry>,
    credentials: Arc<dyn Credentials>,
    controls: Mutex<Controls>,
    boundary: Mutex<Boundary>,
    events: broadcast::Sender<Notification>,
    shutdown: watch::Sender<bool>,
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
        Self(Arc::new(Inner {
            repository,
            providers,
            credentials,
            controls: Mutex::new(Controls::default()),
            shutdown,
            events: broadcast::channel(256).0,
            boundary: Mutex::new(Boundary {
                cursor: Cursor {
                    epoch: Uuid::new_v4().to_string(),
                    seq: 0,
                },
                connections: HashSet::new(),
            }),
        }))
    }
    pub async fn stop(&self) {
        self.0.shutdown.send_replace(true);
        {
            let controls = self.0.controls.lock().await;
            for run in controls.running.values() {
                run.cancel.send_replace(true);
            }
        }
        self.0.providers.shutdown().await;
    }
    pub fn shutdown(&self) -> watch::Receiver<bool> {
        self.0.shutdown.subscribe()
    }
    pub async fn subscribe(&self) -> Subscription {
        let mut boundary = self.0.boundary.lock().await;
        let id = Uuid::new_v4().to_string();
        let receiver = self.0.events.subscribe();
        boundary.connections.insert(id.clone());
        Subscription {
            id,
            cursor: boundary.cursor.clone(),
            receiver,
        }
    }
    pub async fn unsubscribe(&self, id: &str) {
        self.0.boundary.lock().await.connections.remove(id);
    }
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<SnapshotView, Error> {
        let boundary = self.0.boundary.lock().await;
        if subscription.is_some_and(|id| !boundary.connections.contains(id)) {
            return Err(Error::new(ErrorKind::ResyncRequired));
        }
        let mut stored = self.0.repository.snapshot().await?;
        let mut config = self.0.repository.configuration().await?;
        generation::refresh(self, &mut config);
        stored.companion.availability = config.companion.availability;
        let grants = config.grants;
        Ok(SnapshotView {
            grants,
            stored,
            cursor: boundary.cursor.clone(),
        })
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        let companion = self.snapshot(None).await?.stored.companion;
        if companion.id != id {
            return Err(Error::new(ErrorKind::NotFound));
        }
        Ok(companion)
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        Ok(vec![self.snapshot(None).await?.stored.companion])
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        Ok(self.0.repository.snapshot().await?.settings)
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<Grant>, Error> {
        if let Some(id) = companion {
            self.companion(id).await?;
        }
        let mut controls = self.0.controls.lock().await;
        Ok(config(self, &mut controls).await?.grants)
    }
    pub async fn messages(
        &self,
        companion: String,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Message>, Error> {
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
