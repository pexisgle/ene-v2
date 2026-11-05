//! Host functions and per-invocation budgets shared by all instances.
use crate::{Error, Result, contract::Events, limits::*};
use ene_plugin::*;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub trait Services: Send + Sync {
    fn handle(&self, request: ServiceRequest)
    -> BoxFuture<'_, std::result::Result<Value, Failure>>;
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>>;
}
pub struct DenyServices;
impl Services for DenyServices {
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async { Err(Failure::new("permission_required")) })
    }
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

// A cancelled compilation still holds its slot until the native compiler returns.
pub(super) static SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_INSTANCES)));
pub(super) fn ene_service(
    plugin: &mut extism::CurrentPlugin,
    input: &[extism::Val],
    output: &mut [extism::Val],
    _: extism::UserData<()>,
) -> std::result::Result<(), extism::Error> {
    let raw: &[u8] = plugin.memory_get_val(&input[0])?;
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Protocol.into());
    }
    let raw_len = raw.len();
    let request = serde_json::from_slice::<ServiceRequest>(raw)?;
    let invocation = plugin.host_context::<Arc<Invocation>>()?.clone();
    // Charge the bytes actually received and sent, not their re-encoded form.
    invocation.charge(&invocation.bytes, raw_len, MAX_SERVICE_BYTES)?;
    let result = invocation.service(request)?;
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_CHUNK {
        return Err(Error::Protocol.into());
    }
    invocation.charge(&invocation.bytes, bytes.len(), MAX_SERVICE_BYTES)?;
    plugin.memory_set_val(&mut output[0], bytes)
}
pub(super) fn ene_emit(
    plugin: &mut extism::CurrentPlugin,
    input: &[extism::Val],
    _: &mut [extism::Val],
    _: extism::UserData<()>,
) -> std::result::Result<(), extism::Error> {
    let raw: &[u8] = plugin.memory_get_val(&input[0])?;
    if raw.len() > MAX_CHUNK {
        return Err(Error::Protocol.into());
    }
    let raw_len = raw.len();
    let event = serde_json::from_slice::<Event>(raw)?;
    let invocation = plugin.host_context::<Arc<Invocation>>()?.clone();
    invocation.charge(&invocation.stream_bytes, raw_len, MAX_STREAM_BYTES)?;
    invocation.emit(event)?;
    Ok(())
}
pub(super) struct Invocation {
    pub(super) services: Arc<dyn Services>,
    pub(super) declared: Vec<Service>,
    pub(super) calls: AtomicUsize,
    pub(super) bytes: AtomicUsize,
    pub(super) stream_bytes: AtomicUsize,
    pub(super) events: mpsc::Sender<Event>,
    pub(super) validator: Mutex<Events>,
    pub(super) cancel: CancellationToken,
    pub(super) timeout: Duration,
    pub(super) runtime: tokio::runtime::Handle,
}
impl Invocation {
    fn wait<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        self.runtime.block_on(async {
            tokio::select! { biased;
                _ = self.cancel.cancelled() => Err(Error::Cancelled),
                _ = self.events.closed() => Err(Error::Cancelled),
                result = tokio::time::timeout(self.timeout, work) => result.unwrap_or(Err(Error::Timeout)),
            }
        })
    }
    fn service(&self, request: ServiceRequest) -> Result<std::result::Result<Value, Failure>> {
        if self.calls.fetch_add(1, Ordering::Relaxed) >= MAX_SERVICE_CALLS {
            return Err(Error::Protocol);
        }
        self.wait(async {
            self.services.check().await?;
            if !self.declared.contains(&request.service()) {
                return Ok(Err(Failure::new("undeclared_service")));
            }
            let result = self.services.handle(request).await;
            self.services.check().await?;
            Ok(result)
        })
    }
    fn emit(&self, event: Event) -> Result<()> {
        self.validator
            .lock()
            .map_err(|_| Error::Unavailable)?
            .accept(&event)?;
        self.wait(async {
            self.services.check().await?;
            self.events.send(event).await.map_err(|_| Error::Cancelled)
        })
    }
    fn charge(&self, total: &AtomicUsize, bytes: usize, limit: usize) -> Result<()> {
        let total = total
            .fetch_add(bytes, Ordering::Relaxed)
            .checked_add(bytes)
            .ok_or(Error::Protocol)?;
        if total > limit {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
pub(super) struct Cancellation {
    pub(super) token: CancellationToken,
    pub(super) interrupt: extism::CancelHandle,
    pub(super) armed: bool,
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.token.cancel();
        if self.armed {
            let _ = self.interrupt.cancel();
        }
    }
}
