//! Host-owned device authorization and durable operation receipts.
//! Network cancellation does not cancel an admitted write or repeat an operation.
use crate::infrastructure::sqlite::Store;
use buffa::Message;
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

const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024;
// Reserve the largest result plus the nested protobuf tags/length prefixes.
const COMPLETION_RESERVATION: usize = MAX_RESULT_BYTES + 32;

fn journal_budget(journal: &w::DeviceJournal) -> usize {
    journal.encoded_len() as usize
        + journal
            .operations
            .iter()
            .filter(|op| {
                op.state.status == w::OperationStatus::Dispatched
                    || op.state.status == w::OperationStatus::Uncertain
            })
            .count()
            * COMPLETION_RESERVATION
}

#[derive(Clone)]
pub(crate) struct Devices(Arc<Inner>);
struct Inner {
    state: Mutex<State>,
    store: Option<Store>,
    tasks: TaskTracker,
    slots: Arc<Semaphore>,
}
#[derive(Default)]
struct State {
    loaded: bool,
    failed: bool,
    activation_not_before: Option<Instant>,
    journal: w::DeviceJournal,
    connections: HashMap<String, Connection>,
    changes: HashMap<String, CancellationToken>,
    activations: HashMap<String, (w::Activation, Instant)>,
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
fn device<'a>(journal: &'a w::DeviceJournal, id: &str) -> Result<&'a w::Device, ConnectError> {
    journal
        .devices
        .iter()
        .find_map(|r| r.device.as_option().filter(|d| d.id == id))
        .ok_or_else(missing)
}
fn device_mut<'a>(
    journal: &'a mut w::DeviceJournal,
    id: &str,
) -> Result<&'a mut w::Device, ConnectError> {
    journal
        .devices
        .iter_mut()
        .find_map(|r| r.device.as_option_mut().filter(|d| d.id == id))
        .ok_or_else(missing)
}
fn permitted(
    journal: &w::DeviceJournal,
    id: &str,
    capability: Option<&str>,
) -> Result<(), ConnectError> {
    let d = device(journal, id)?;
    if !d.approved || d.revoked || capability.is_some_and(|c| !d.permissions.iter().any(|p| p == c))
    {
        return Err(denied());
    }
    Ok(())
}
impl Devices {
    pub fn new(store: Option<Store>) -> Self {
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
            if let Some(store) = &self.0.store {
                if let Some(bytes) = store.rpc_state().await.map_err(|_| unavailable())? {
                    state.journal =
                        w::DeviceJournal::decode_from_slice(&bytes).map_err(|_| unavailable())?;
                }
                if state.journal.activation_revision > 0 {
                    state.activation_not_before = Some(Instant::now() + Duration::from_secs(15));
                }
                for operation in &mut state.journal.operations {
                    if operation.state.status == w::OperationStatus::Dispatched {
                        operation
                            .state
                            .as_option_mut()
                            .ok_or_else(unavailable)?
                            .status = w::OperationStatus::Uncertain.into();
                    }
                }
                store
                    .save_rpc_state(state.journal.encode_to_vec())
                    .await
                    .map_err(|_| unavailable())?;
            }
            state.loaded = true;
        }
        Ok(state)
    }
    async fn commit(
        &self,
        state: &mut State,
        journal: w::DeviceJournal,
    ) -> Result<(), ConnectError> {
        let budget = journal_budget(&journal);
        // Older hosts admitted operations without reservations. Allow their
        // completion/revocation to reduce an over-budget journal.
        if journal.encoded_len() as usize > MAX_JOURNAL_BYTES
            || (budget > MAX_JOURNAL_BYTES && budget > journal_budget(&state.journal))
        {
            return Err(ConnectError::resource_exhausted(
                "端末操作履歴の容量上限です",
            ));
        }
        if let Some(store) = &self.0.store
            && store.save_rpc_state(journal.encode_to_vec()).await.is_err()
        {
            state.failed = true;
            for signal in state.changes.values() {
                signal.cancel();
            }
            return Err(unavailable());
        }
        state.journal = journal;
        Ok(())
    }
    pub async fn authenticate(&self, token: &str) -> Option<String> {
        let hash = Sha256::digest(token.as_bytes());
        let state = self.lock().await.ok()?;
        state
            .journal
            .devices
            .iter()
            .find(|r| {
                bool::from(hash.as_slice().ct_eq(&r.token_hash))
                    && r.device.approved
                    && !r.device.revoked
            })
            .map(|r| r.device.id.clone())
    }
}
mod operations;
mod registration;
#[cfg(test)]
mod tests;
