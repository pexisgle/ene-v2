//! Host functions and per-invocation budgets shared by all instances.
use super::json::{self, BoundedJson};
use crate::plugin_runtime::{contract::Events, limits::*};
use crate::*;
use extism::convert::FromBytesOwned;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::{
    Arc, LazyLock, Weak,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

pub trait Services: Send + Sync {
    fn handle(&self, request: ServiceRequest)
    -> BoxFuture<'_, std::result::Result<Value, Failure>>;
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}
#[derive(Debug)]
pub struct DenyServices;
impl Services for DenyServices {
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async { Err(Failure::new("permission_required")) })
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
    let invocation = context
        .lock()
        .upgrade()
        .ok_or(anyhow::anyhow!("plugin_unavailable"))?;
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
    let invocation = context
        .lock()
        .upgrade()
        .ok_or(anyhow::anyhow!("plugin_unavailable"))?;
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
    pub(super) callbacks: mpsc::Sender<Callback>,
}
type ServiceResult = crate::plugin_runtime::Result<std::result::Result<Value, Failure>>;

// Extism's callbacks run synchronously on the native worker. At most one
// request is outstanding, and dropping the invocation closes its response.
pub(super) enum Callback {
    Service(
        ServiceRequest,
        oneshot::Sender<ServiceResult>,
        tracing::Span,
    ),
    Emit(
        Box<Event>,
        oneshot::Sender<crate::plugin_runtime::Result<()>>,
        tracing::Span,
    ),
}

impl Invocation {
    pub(super) async fn serve(&self, mut callbacks: mpsc::Receiver<Callback>) {
        while let Some(callback) = callbacks.recv().await {
            match callback {
                Callback::Service(request, reply, span) => {
                    let result = self.handle_service(request).instrument(span).await;
                    let _sent = reply.send(result);
                }
                Callback::Emit(event, reply, span) => {
                    let result = self.send_event(*event).instrument(span).await;
                    let _sent = reply.send(result);
                }
            }
        }
    }

    fn request<T>(
        &self,
        callback: impl FnOnce(
            oneshot::Sender<crate::plugin_runtime::Result<T>>,
            tracing::Span,
        ) -> Callback,
    ) -> crate::plugin_runtime::Result<T> {
        let (reply, response) = oneshot::channel();
        self.callbacks
            .try_send(callback(reply, tracing::Span::current()))
            .map_err(|_| anyhow::anyhow!("cancelled"))?;
        response
            .blocking_recv()
            .map_err(|_| anyhow::anyhow!("cancelled"))?
    }

    #[tracing::instrument(name = "wasm.host_service", skip_all, fields(service = ?request.service()))]
    fn service(&self, request: ServiceRequest) -> ServiceResult {
        if self.calls.fetch_add(1, Ordering::Relaxed) >= MAX_SERVICE_CALLS {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        self.request(|reply, span| Callback::Service(request, reply, span))
    }

    async fn handle_service(&self, request: ServiceRequest) -> ServiceResult {
        self.services.check().await?;
        if !self.declared.contains(&request.service()) {
            return Ok(Err(Failure::new("undeclared_service")));
        }
        let result = self.services.handle(request).await;
        self.services.check().await?;
        Ok(result)
    }

    #[tracing::instrument(name = "wasm.host_emit", skip_all)]
    fn emit(&self, event: Event) -> crate::plugin_runtime::Result<()> {
        self.validator.lock().accept(&event)?;
        self.request(|reply, span| Callback::Emit(Box::new(event), reply, span))
    }

    async fn send_event(&self, event: Event) -> crate::plugin_runtime::Result<()> {
        self.services.check().await?;
        self.events
            .send(event)
            .await
            .map_err(|_| anyhow::anyhow!("cancelled"))
    }

    fn charge(
        &self,
        total: &AtomicUsize,
        bytes: usize,
        limit: usize,
    ) -> crate::plugin_runtime::Result<()> {
        let total = total
            .fetch_add(bytes, Ordering::Relaxed)
            .checked_add(bytes)
            .ok_or(anyhow::anyhow!("invalid_plugin_contract"))?;
        if total > limit {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
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
