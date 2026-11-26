//! Host functions and per-invocation budgets shared by all instances.
use super::json::{self, BoundedJson};
use crate::{Error, contract::Events, limits::*};
use ene_plugin::*;
use extism::convert::FromBytesOwned;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub trait Services: Send + Sync {
    fn handle(&self, request: ServiceRequest)
    -> BoxFuture<'_, std::result::Result<Value, Failure>>;
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>>;
}
#[derive(Debug)]
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
pub(super) fn host_functions(context: &Arc<Mutex<Weak<Invocation>>>) -> [extism::Function; 2] {
    let service_context = context.clone();
    let emit_context = context.clone();
    [
        extism::Function::new(
            "ene_service",
            [extism::PTR],
            [extism::PTR],
            extism::UserData::<()>::default(),
            move |plugin, inputs, outputs, _| {
                let raw = plugin.memory_get_val(&inputs[0])?;
                let bytes = ene_service(&service_context, raw)?;
                let output = plugin.memory_new(&bytes)?;
                outputs[0] = plugin.memory_to_val(output);
                Ok(())
            },
        ),
        extism::Function::new(
            "ene_emit",
            [extism::PTR],
            [],
            extism::UserData::<()>::default(),
            move |plugin, inputs, _, _| {
                let raw = plugin.memory_get_val(&inputs[0])?;
                ene_emit(&emit_context, raw)
            },
        ),
    ]
}

fn ene_service(context: &Mutex<Weak<Invocation>>, raw: &[u8]) -> Result<Vec<u8>, extism::Error> {
    let raw_len = raw.len();
    let request = BoundedJson::<ServiceRequest, MAX_MESSAGE_BYTES>::from_bytes_owned(raw)?.0;
    let invocation = context.lock().upgrade().ok_or(Error::Unavailable)?;
    // Charge the bytes actually received and sent, not their re-encoded form.
    invocation.charge(&invocation.bytes, raw_len, MAX_SERVICE_BYTES)?;
    let result = invocation.service(request)?;
    let bytes = json::encode::<_, MAX_CHUNK>(result)?;
    invocation.charge(&invocation.bytes, bytes.len(), MAX_SERVICE_BYTES)?;
    Ok(bytes)
}

fn ene_emit(context: &Mutex<Weak<Invocation>>, raw: &[u8]) -> Result<(), extism::Error> {
    let raw_len = raw.len();
    let event = BoundedJson::<Event, MAX_CHUNK>::from_bytes_owned(raw)?.0;
    let invocation = context.lock().upgrade().ok_or(Error::Unavailable)?;
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
    pub(super) runtime: tokio::runtime::Handle,
}
impl Invocation {
    fn wait<T>(&self, work: impl Future<Output = crate::Result<T>>) -> crate::Result<T> {
        self.runtime.block_on(async {
            tokio::select! { biased;
                _ = self.cancel.cancelled() => Err(Error::Cancelled),
                _ = self.events.closed() => Err(Error::Cancelled),
                // The enclosing call timeout drops Cancellation and cancels this token.
                result = work => result,
            }
        })
    }
    #[tracing::instrument(name = "wasm.host_service", skip_all, fields(service = ?request.service()))]
    fn service(
        &self,
        request: ServiceRequest,
    ) -> crate::Result<std::result::Result<Value, Failure>> {
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
    #[tracing::instrument(name = "wasm.host_emit", skip_all)]
    fn emit(&self, event: Event) -> crate::Result<()> {
        self.validator.lock().accept(&event)?;
        self.wait(async {
            self.services.check().await?;
            self.events.send(event).await.map_err(|_| Error::Cancelled)
        })
    }
    fn charge(&self, total: &AtomicUsize, bytes: usize, limit: usize) -> crate::Result<()> {
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
            // `CancellationToken::cancel` only fails when no listeners remain,
            // which is exactly the state `Drop` cannot act on.
            let _cancelled = self.interrupt.cancel();
        }
    }
}
