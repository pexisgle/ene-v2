use crate::store::Store;
use ene_shared::host::{Cursor, Notification};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::{Mutex, broadcast, watch};
use uuid::Uuid;

pub struct EventBoundary {
    pub cursor: Cursor,
    pub connections: HashSet<String>,
}

#[derive(Clone)]
pub struct App {
    pub(crate) token: Arc<str>,
    pub store: Store,
    pub boundary: Arc<Mutex<EventBoundary>>,
    pub events: broadcast::Sender<Notification>,
    pub shutdown: watch::Sender<bool>,
}

impl App {
    pub fn new(store: Store, token: String) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            token: token.into(),
            store,
            events,
            shutdown: watch::channel(false).0,
            boundary: Arc::new(Mutex::new(EventBoundary {
                cursor: Cursor {
                    epoch: Uuid::new_v4().to_string(),
                    seq: 0,
                },
                connections: HashSet::new(),
            })),
        }
    }
}
