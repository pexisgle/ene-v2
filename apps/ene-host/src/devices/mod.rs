//! Host-owned device operation delivery and durable receipts. Each record is
//! one SQLite row; the live streams and activation leases are the only state
//! the host keeps. Network cancellation does not cancel an admitted write or
//! repeat an operation.
use crate::infrastructure::sqlite::Store;
use connectrpc::ConnectError;
use ene_rpc::rpc as w;
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use tokio::sync::{Mutex, MutexGuard, Semaphore, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_OPERATIONS: i64 = 2048;

#[derive(Clone)]
pub(crate) struct Devices(Arc<Inner>);
struct Inner {
    state: Mutex<State>,
    store: Store,
    tasks: TaskTracker,
    slots: Arc<Semaphore>,
}
#[derive(Default)]
struct State {
    loaded: bool,
    failed: bool,
    /// A restart makes an older activation unusable for a moment, so a
    /// client cannot keep a lease it did not renew.
    activation_not_before: Option<Instant>,
    activation_revision: u64,
    /// One entry per device ID asserted by an authenticated client: its live
    /// operation stream, if any, and the signal that ends that device's stream.
    sessions: HashMap<String, DeviceSession>,
    activations: HashMap<String, (w::Activation, Instant)>,
}
#[derive(Default)]
struct DeviceSession {
    connection: Option<Connection>,
    changed: CancellationToken,
}
struct Connection {
    id: String,
    queue: mpsc::Sender<w::DeviceOperation>,
}
pub(crate) struct Subscription {
    pub id: String,
    pub queue: mpsc::Receiver<w::DeviceOperation>,
    pub changed: CancellationToken,
}
fn missing() -> ConnectError {
    ConnectError::not_found("端末または操作が見つかりません")
}
fn conflict() -> ConnectError {
    ConnectError::failed_precondition("端末の状態が変わりました")
}
fn unavailable() -> ConnectError {
    ConnectError::unavailable("端末の保存処理を利用できません")
}
/// Stops serving device actions when the store can no longer prove the device
/// state it last read. A failed operation write must never leave the affected
/// streams running.
fn fail_closed(state: &mut State) -> ConnectError {
    state.failed = true;
    for session in state.sessions.values() {
        session.changed.cancel();
    }
    unavailable()
}
fn validate(value: &str) -> Result<(), ConnectError> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        Err(ConnectError::invalid_argument("識別子が不正です"))
    } else {
        Ok(())
    }
}
impl Devices {
    pub fn new(store: Store) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            store,
            tasks: TaskTracker::new(),
            slots: Arc::new(Semaphore::new(64)),
        }))
    }
    pub async fn stop(&self) {
        self.0.slots.close();
        self.0.tasks.close();
        self.0.tasks.wait().await;
    }
    async fn owned<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, ConnectError>> + Send + 'static,
    ) -> Result<T, ConnectError> {
        let slot = self
            .0
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ConnectError::resource_exhausted("端末操作が混雑しています"))?;
        self.0
            .tasks
            .spawn(async move {
                let _slot = slot;
                future.await
            })
            .await
            .map_err(|_| unavailable())?
    }
    async fn lock(&self) -> Result<MutexGuard<'_, State>, ConnectError> {
        let mut state = self.0.state.lock().await;
        if state.failed {
            return Err(unavailable());
        }
        if !state.loaded {
            // A write that failed leaves the store uncertain; the host serves
            // no device action until it restarts.
            state.activation_revision = self
                .0
                .store
                .device_recover()
                .await
                .map_err(|_| unavailable())?;
            if state.activation_revision > 0 {
                state.activation_not_before = Some(Instant::now() + Duration::from_secs(15));
            }
            state.loaded = true;
        }
        Ok(state)
    }
    /// Opens the one operation stream for a client-asserted device ID.
    pub async fn subscribe(&self, id: String) -> Result<Subscription, ConnectError> {
        validate(&id)?;
        let mut state = self.lock().await?;
        let entry = state.sessions.entry(id).or_default();
        if entry
            .connection
            .as_ref()
            .is_some_and(|c| !c.queue.is_closed())
        {
            return Err(conflict());
        }
        let session = Uuid::new_v4().to_string();
        let (queue, incoming) = mpsc::channel(32);
        let changed = entry.changed.clone();
        entry.connection = Some(Connection {
            id: session.clone(),
            queue,
        });
        Ok(Subscription {
            id: session,
            queue: incoming,
            changed,
        })
    }
}
mod operations;
#[cfg(test)]
mod tests;
