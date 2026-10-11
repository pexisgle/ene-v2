//! Host-owned device authorization and durable operation receipts.
//! Network cancellation does not cancel an admitted write or repeat an operation.
use crate::infrastructure::sqlite::Store;
use buffa::Message;
use connectrpc::ConnectError;
use ene_rpc::rpc as w;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Weak},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, MutexGuard, Semaphore, mpsc, watch};
use tokio::time::Instant;
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
    changes: HashMap<String, watch::Sender<()>>,
    activations: HashMap<String, (w::Activation, Instant)>,
}
struct Connection {
    id: String,
    queue: mpsc::Sender<w::DeviceOperation>,
    lease: Weak<()>,
}
pub(crate) struct Subscription {
    pub id: String,
    pub queue: mpsc::Receiver<w::DeviceOperation>,
    pub lease: Arc<()>,
    pub changed: watch::Receiver<()>,
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
    pub async fn initialize(&self) -> Result<(), ConnectError> {
        let _state = self.lock().await?;
        Ok(())
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
                signal.send_replace(());
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
    pub async fn register(
        &self,
        request: w::RegisterDeviceRequest,
    ) -> Result<w::DeviceRegistration, ConnectError> {
        validate(&request.name)?;
        if request.capabilities.len() > 32 {
            return Err(ConnectError::resource_exhausted("端末能力が多すぎます"));
        }
        for c in &request.capabilities {
            validate(c)?;
        }
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            if state.journal.devices.len() >= 64 {
                return Err(ConnectError::resource_exhausted("端末数の上限です"));
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let d = w::Device {
                id: Uuid::new_v4().to_string(),
                name: request.name,
                capabilities: request.capabilities,
                ..Default::default()
            };
            let mut journal = state.journal.clone();
            journal.devices.push(w::DeviceRecord {
                device: d.clone().into(),
                token_hash: Sha256::digest(token.as_bytes()).to_vec(),
                ..Default::default()
            });
            this.commit(&mut state, journal).await?;
            Ok(w::DeviceRegistration {
                device: d.into(),
                token,
                ..Default::default()
            })
        })
        .await
    }
    pub async fn list(&self) -> Result<w::Devices, ConnectError> {
        Ok(w::Devices {
            items: self
                .lock()
                .await?
                .journal
                .devices
                .iter()
                .map(|d| (*d.device).clone())
                .collect(),
            ..Default::default()
        })
    }
    pub async fn approve(&self, id: String) -> Result<w::Device, ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let mut journal = state.journal.clone();
            let d = device_mut(&mut journal, &id)?;
            if d.revoked {
                return Err(conflict());
            }
            d.approved = true;
            let result = d.clone();
            this.commit(&mut state, journal).await?;
            Ok(result)
        })
        .await
    }
    pub async fn revoke(&self, id: String) -> Result<(), ConnectError> {
        self.permission(id, None, false).await
    }
    pub async fn permission(
        &self,
        id: String,
        capability: Option<String>,
        allowed: bool,
    ) -> Result<(), ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let mut journal = state.journal.clone();
            let d = device_mut(&mut journal, &id)?;
            if let Some(c) = &capability {
                if d.revoked || !d.capabilities.contains(c) {
                    return Err(denied());
                }
                d.permissions.retain(|p| p != c);
                if allowed {
                    d.permissions.push(c.clone());
                }
            } else {
                d.approved = false;
                d.revoked = true;
                d.permissions.clear();
            }
            if !allowed {
                for op in &mut journal.operations {
                    if op.request.device_id == id
                        && capability
                            .as_ref()
                            .is_none_or(|c| op.request.capability == *c)
                        && op.state.status == w::OperationStatus::Dispatched
                    {
                        op.state.as_option_mut().ok_or_else(unavailable)?.status =
                            w::OperationStatus::Revoked.into();
                    }
                }
            }
            this.commit(&mut state, journal).await?;
            if let Some(signal) = state.changes.get(&id) {
                signal.send_replace(());
            }
            if !allowed {
                state.connections.remove(&id);
            }
            Ok(())
        })
        .await
    }
    pub async fn permitted(&self, id: &str, capability: &str) -> Result<(), ConnectError> {
        permitted(&self.lock().await?.journal, id, Some(capability))
    }
    pub async fn changes(&self, id: &str) -> Result<watch::Receiver<()>, ConnectError> {
        let mut state = self.lock().await?;
        permitted(&state.journal, id, None)?;
        Ok(state
            .changes
            .entry(id.into())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe())
    }
    pub async fn subscribe(&self, id: String) -> Result<Subscription, ConnectError> {
        let mut state = self.lock().await?;
        permitted(&state.journal, &id, None)?;
        if state
            .connections
            .get(&id)
            .is_some_and(|c| c.lease.strong_count() > 0)
        {
            return Err(conflict());
        }
        let session = Uuid::new_v4().to_string();
        let lease = Arc::new(());
        let (queue, incoming) = mpsc::channel(32);
        let changed = state
            .changes
            .entry(id.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        state.connections.insert(
            id,
            Connection {
                id: session.clone(),
                queue,
                lease: Arc::downgrade(&lease),
            },
        );
        Ok(Subscription {
            id: session,
            queue: incoming,
            lease,
            changed,
        })
    }
    pub async fn request(
        &self,
        request: w::DeviceOperationRequest,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        validate(&request.operation_id)?;
        validate(&request.capability)?;
        if request.encoded_len() > 64 * 1024
            || !request.input.as_option().is_some_and(|document| {
                serde_json::from_str::<serde_json::Value>(&document.json).is_ok()
            })
        {
            return Err(ConnectError::invalid_argument("端末操作の入力が不正です"));
        }
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            if let Some(op) = state
                .journal
                .operations
                .iter()
                .find(|o| o.request.operation_id == request.operation_id)
            {
                if *op.request != request {
                    return Err(conflict());
                }
                return Ok(this.current(&state, op));
            }
            permitted(
                &state.journal,
                &request.device_id,
                Some(&request.capability),
            )?;
            if state.journal.operations.len() >= 2048 {
                return Err(ConnectError::resource_exhausted("端末操作履歴の上限です"));
            }
            let connection = state
                .connections
                .get(&request.device_id)
                .filter(|c| c.lease.strong_count() > 0)
                .ok_or_else(conflict)?;
            let permit = connection
                .queue
                .clone()
                .try_reserve_owned()
                .map_err(|_| ConnectError::resource_exhausted("端末操作キューが満杯です"))?;
            let subscription_id = connection.id.clone();
            let result = w::DeviceOperationState {
                operation_id: request.operation_id.clone(),
                status: w::OperationStatus::Dispatched.into(),
                ..Default::default()
            };
            let command = w::DeviceOperation {
                operation_id: request.operation_id.clone(),
                capability: request.capability.clone(),
                input: request.input.clone(),
                subscription_id: subscription_id.clone(),
                ..Default::default()
            };
            let mut journal = state.journal.clone();
            journal.operations.push(w::OperationRecord {
                request: request.into(),
                state: result.clone().into(),
                subscription_id,
                ..Default::default()
            });
            this.commit(&mut state, journal).await?;
            permit.send(command);
            Ok(result)
        })
        .await
    }
    fn current(&self, state: &State, op: &w::OperationRecord) -> w::DeviceOperationState {
        let mut result = (*op.state).clone();
        if result.status == w::OperationStatus::Dispatched
            && !state
                .connections
                .get(&op.request.device_id)
                .is_some_and(|c| c.id == op.subscription_id && c.lease.strong_count() > 0)
        {
            result.status = w::OperationStatus::Uncertain.into();
        }
        result
    }
    pub async fn operation(
        &self,
        device_id: &str,
        id: &str,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        let state = self.lock().await?;
        let op = state
            .journal
            .operations
            .iter()
            .find(|o| o.request.operation_id == id && o.request.device_id == device_id)
            .ok_or_else(missing)?;
        Ok(self.current(&state, op))
    }
    pub async fn complete(
        &self,
        result: w::DeviceOperationResult,
    ) -> Result<w::DeviceOperationState, ConnectError> {
        if result.encoded_len() as usize > MAX_RESULT_BYTES
            || !result.output.as_option().is_some_and(|document| {
                serde_json::from_str::<serde_json::Value>(&document.json).is_ok()
            })
        {
            return Err(ConnectError::invalid_argument("端末操作の結果が不正です"));
        }
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            let index = state
                .journal
                .operations
                .iter()
                .position(|o| {
                    o.request.operation_id == result.operation_id
                        && o.request.device_id == result.device_id
                })
                .ok_or_else(missing)?;
            let previous = &state.journal.operations[index];
            if previous.state.status == w::OperationStatus::Completed {
                if previous.state.result.as_option() == Some(&result) {
                    return Ok((*previous.state).clone());
                }
                return Err(conflict());
            }
            permitted(
                &state.journal,
                &result.device_id,
                Some(&previous.request.capability),
            )?;
            if this.current(&state, previous).status != w::OperationStatus::Dispatched
                || previous.subscription_id != result.subscription_id
            {
                return Err(conflict());
            }
            let response = w::DeviceOperationState {
                operation_id: result.operation_id.clone(),
                status: w::OperationStatus::Completed.into(),
                result: result.into(),
                ..Default::default()
            };
            let mut journal = state.journal.clone();
            journal.operations[index].state = response.clone().into();
            this.commit(&mut state, journal).await?;
            Ok(response)
        })
        .await
    }
    pub async fn activate(
        &self,
        request: w::ActivationRequest,
    ) -> Result<w::Activation, ConnectError> {
        let this = self.clone();
        self.owned(async move {
            let mut state = this.lock().await?;
            permitted(
                &state.journal,
                &request.device_id,
                Some("companion.display"),
            )?;
            if state
                .activation_not_before
                .is_some_and(|deadline| deadline > Instant::now())
            {
                return Err(conflict());
            }
            let old = state
                .activations
                .get(&request.companion_id)
                .filter(|(_, deadline)| *deadline > Instant::now());
            if let Some((old, _)) = old {
                if old.device_id != request.device_id
                    || request.expected_revision != Some(old.revision)
                {
                    return Err(conflict());
                }
            } else if request.release {
                return Err(conflict());
            }
            let mut journal = state.journal.clone();
            journal.activation_revision = journal
                .activation_revision
                .checked_add(1)
                .ok_or_else(unavailable)?;
            let result = w::Activation {
                companion_id: request.companion_id,
                device_id: request.device_id,
                revision: journal.activation_revision,
                lease_ms: if request.release { 0 } else { 15_000 },
                ..Default::default()
            };
            this.commit(&mut state, journal).await?;
            if request.release {
                state.activations.remove(&result.companion_id);
            } else {
                state.activations.insert(
                    result.companion_id.clone(),
                    (
                        result.clone(),
                        Instant::now() + Duration::from_millis(result.lease_ms.into()),
                    ),
                );
            }
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connected() -> (Devices, String, Subscription) {
        let devices = Devices::new(None);
        let registration = devices
            .register(w::RegisterDeviceRequest {
                name: "fixture".into(),
                capabilities: vec!["write".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        let id = registration.device.id.clone();
        devices.approve(id.clone()).await.unwrap();
        devices
            .permission(id.clone(), Some("write".into()), true)
            .await
            .unwrap();
        let subscription = devices.subscribe(id.clone()).await.unwrap();
        (devices, id, subscription)
    }
    fn request(id: &str, operation: &str) -> w::DeviceOperationRequest {
        w::DeviceOperationRequest {
            device_id: id.into(),
            operation_id: operation.into(),
            capability: "write".into(),
            input: w::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }
    async fn fill(devices: &Devices, remaining: usize) {
        let mut state = devices.lock().await.unwrap();
        // Represent a full persisted journal without thousands of test operations.
        state.journal.devices[0]
            .device
            .as_option_mut()
            .unwrap()
            .name = "x".repeat(MAX_JOURNAL_BYTES - remaining - 512);
    }
    #[tokio::test]
    async fn admitted_write_can_persist_a_maximum_result_near_capacity() {
        let (devices, id, mut subscription) = connected().await;
        fill(&devices, COMPLETION_RESERVATION + 512).await;
        devices.request(request(&id, "first")).await.unwrap();
        let operation = subscription.queue.recv().await.unwrap();
        let mut result = w::DeviceOperationResult {
            device_id: id.clone(),
            operation_id: operation.operation_id,
            subscription_id: operation.subscription_id,
            success: true,
            output: w::JsonDocument {
                json: "\"\"".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let overhead = result.encoded_len() as usize;
        result.output.as_option_mut().unwrap().json =
            format!("\"{}\"", "x".repeat(MAX_RESULT_BYTES - overhead - 4));
        assert!(result.encoded_len() as usize <= MAX_RESULT_BYTES);
        assert!(result.encoded_len() as usize >= MAX_RESULT_BYTES - 8);
        let completed = devices.complete(result.clone()).await.unwrap();
        assert_eq!(completed.status, w::OperationStatus::Completed);
        assert_eq!(devices.complete(result).await.unwrap(), completed);
        assert_eq!(devices.operation(&id, "first").await.unwrap(), completed);
    }
    #[tokio::test]
    async fn insufficient_completion_capacity_rejects_before_delivery() {
        let (devices, id, mut subscription) = connected().await;
        fill(&devices, 8 * 1024).await;
        assert!(devices.request(request(&id, "first")).await.is_err());
        assert!(matches!(
            subscription.queue.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(devices.lock().await.unwrap().journal.operations.is_empty());
    }
    #[tokio::test]
    async fn other_writes_cannot_consume_an_admitted_operations_reservation() {
        let (devices, id, mut subscription) = connected().await;
        fill(&devices, COMPLETION_RESERVATION + 512).await;
        devices.request(request(&id, "first")).await.unwrap();
        assert!(devices.request(request(&id, "second")).await.is_err());
        assert_eq!(
            subscription.queue.recv().await.unwrap().operation_id,
            "first"
        );
        assert!(subscription.queue.try_recv().is_err());
        assert_eq!(devices.lock().await.unwrap().journal.operations.len(), 1);
    }
}
