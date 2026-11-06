//! Host-owned device authorization and durable operation receipts. Each record
//! is one SQLite row; the live streams and activation leases are the only
//! state the host keeps. Network cancellation does not cancel an admitted
//! write or repeat an operation.
use crate::infrastructure::sqlite::Store;
use connectrpc::ConnectError;
use ene_rpc::rpc as w;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, MutexGuard, Semaphore, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_OPERATIONS: i64 = 2048;
const MAX_DEVICES: usize = 64;

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
    /// One entry per authenticated device: its live operation stream, if any,
    /// and the cancellation signal that ends that device's RPC streams.
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
fn denied() -> ConnectError {
    ConnectError::permission_denied("端末操作の許可が必要です")
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
fn validate(value: &str) -> Result<(), ConnectError> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        Err(ConnectError::invalid_argument("識別子が不正です"))
    } else {
        Ok(())
    }
}
/// Whether `device` may run `capability` now.
fn permitted(device: &w::Device, capability: Option<&str>) -> Result<(), ConnectError> {
    if !device.approved
        || device.revoked
        || capability.is_some_and(|c| !device.permissions.iter().any(|p| p == c))
    {
        return Err(denied());
    }
    Ok(())
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
    async fn device(&self, id: &str) -> Result<w::Device, ConnectError> {
        self.0
            .store
            .device_get(id.into())
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(missing)
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
    /// The authenticated device and the signal that ends its streams.
    pub async fn authenticate(&self, token: &str) -> Option<(String, CancellationToken)> {
        let hash = Sha256::digest(token.as_bytes());
        let mut state = self.lock().await.ok()?;
        let id = self
            .0
            .store
            .device_records()
            .await
            .ok()?
            .into_iter()
            .find(|(device, token_hash)| {
                bool::from(hash.as_slice().ct_eq(token_hash)) && device.approved && !device.revoked
            })
            .map(|(device, _)| device.id)?;
        let session = state.sessions.entry(id.clone()).or_default();
        Some((id, session.changed.clone()))
    }
}
mod operations;
mod registration;
#[cfg(test)]
mod tests;
