//! Extism instances execute on Tokio's blocking pool; host capabilities stay explicit.
use crate::{Error, Package, Result, contract::Events, limits::*};
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
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
static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_INSTANCES)));
type ActiveCall = Arc<Mutex<Option<Arc<Invocation>>>>;
struct Invocation {
    services: Arc<dyn Services>,
    declared: Vec<Service>,
    calls: AtomicUsize,
    events: mpsc::Sender<Event>,
    validator: Mutex<Events>,
    cancel: CancellationToken,
    runtime: tokio::runtime::Handle,
}
impl Invocation {
    fn wait<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        self.runtime.block_on(async {
            tokio::select! { biased;
                _ = self.cancel.cancelled() => Err(Error::Cancelled),
                _ = self.events.closed() => Err(Error::Cancelled),
                result = tokio::time::timeout(EXECUTION_TIMEOUT, work) => result.unwrap_or(Err(Error::Timeout)),
            }
        })
    }
    fn service(&self, request: ServiceRequest) -> Result<std::result::Result<Value, Failure>> {
        crate::contract::service_request(&request)?;
        if self.calls.fetch_add(1, Ordering::Relaxed) >= MAX_SERVICE_CALLS {
            return Err(Error::Protocol);
        }
        self.wait(async {
            self.services.check().await.map_err(|_| Error::Denied)?;
            if !self.declared.contains(&request.service()) {
                return Ok(Err(Failure::new("undeclared_service")));
            }
            let result = self.services.handle(request).await;
            self.services.check().await.map_err(|_| Error::Denied)?;
            Ok(result)
        })
    }
    fn emit(&self, event: Event) -> Result<()> {
        self.validator
            .lock()
            .map_err(|_| Error::Unavailable)?
            .accept(&event)?;
        self.wait(async {
            self.services.check().await.map_err(|_| Error::Denied)?;
            self.events.send(event).await.map_err(|_| Error::Cancelled)
        })
    }
}
fn active(slot: &ActiveCall) -> Result<Arc<Invocation>> {
    slot.lock()
        .map_err(|_| Error::Unavailable)?
        .clone()
        .ok_or(Error::Denied)
}
struct Cancellation {
    token: CancellationToken,
    interrupt: extism::CancelHandle,
    armed: bool,
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.token.cancel();
        if self.armed {
            let _ = self.interrupt.cancel();
        }
    }
}

pub struct Instance {
    plugin: Option<extism::Plugin>,
    active: ActiveCall,
    package: Package,
    entrypoint: String,
    generation: u64,
    permit: Arc<OwnedSemaphorePermit>,
    pub capabilities: Value,
}
impl Instance {
    pub async fn load(
        package: Package,
        entrypoint: String,
        target: Target,
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        package.verify()?;
        let entry = package.entry(&entrypoint)?;
        if entry.target != target {
            return Err(Error::Unsupported);
        }
        let code = package.code(&entry.file)?.to_vec();
        let permit = SLOTS
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Unavailable)?;
        let token = cancel.child_token();
        let stop = token.clone().drop_guard();
        let build = tokio::task::spawn_blocking(move || {
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let slot: ActiveCall = Arc::default();
            let service = slot.clone();
            let emit = slot.clone();
            let manifest = extism::Manifest::new([extism::Wasm::data(code)])
                .with_allowed_hosts(std::iter::empty())
                .with_memory_max(WASM_MEMORY_PAGES)
                .with_timeout(EXECUTION_TIMEOUT);
            let plugin = extism::PluginBuilder::new(manifest)
                .with_wasi(false)
                .with_fuel_limit(WASM_FUEL)
                .with_initialization_fuel_limit(WASM_INITIALIZATION_FUEL)
                .with_function(
                    "ene_service",
                    [extism::ValType::I64],
                    [extism::ValType::I64],
                    extism::UserData::new(()),
                    move |plugin, input, output, _| {
                        let raw: &[u8] = plugin.memory_get_val(&input[0])?;
                        if raw.len() > MAX_CHUNK {
                            return Err(Error::Protocol.into());
                        }
                        let request = serde_json::from_slice(raw)?;
                        let result = active(&service)?.service(request)?;
                        let bytes = serde_json::to_vec(&result)?;
                        if bytes.len() > MAX_CHUNK {
                            return Err(Error::Protocol.into());
                        }
                        plugin.memory_set_val(&mut output[0], bytes)
                    },
                )
                .with_function(
                    "ene_emit",
                    [extism::ValType::I64],
                    [],
                    extism::UserData::new(()),
                    move |plugin, input, _, _| {
                        let raw: &[u8] = plugin.memory_get_val(&input[0])?;
                        if raw.len() > MAX_CHUNK {
                            return Err(Error::Protocol.into());
                        }
                        active(&emit)?.emit(serde_json::from_slice(raw)?)?;
                        Ok(())
                    },
                )
                .build()
                .map_err(|_| Error::Execution)?;
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            Ok(Self {
                plugin: Some(plugin),
                active: slot,
                package,
                entrypoint,
                generation,
                permit: Arc::new(permit),
                capabilities: Value::Null,
            })
        });
        let mut instance = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, build) => result.map_err(|_| Error::Timeout)?.map_err(|_| Error::Execution)??,
        };
        drop(stop);
        instance.capabilities = instance
            .call(
                "ene_initialize",
                b"{}".to_vec(),
                None,
                cancel,
                INITIALIZE_TIMEOUT,
            )
            .await?;
        if instance
            .package
            .entry(&instance.entrypoint)?
            .provides
            .contains(&Extension::Llm)
        {
            let capabilities: LlmCapabilities =
                serde_json::from_value(instance.capabilities.clone())?;
            if !capabilities.streaming_text {
                return Err(Error::Unsupported);
            }
        }
        Ok(instance)
    }
    async fn call(
        &mut self,
        function: &'static str,
        input: Vec<u8>,
        invocation: Option<Arc<Invocation>>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if input.len() > MAX_MESSAGE_BYTES {
            return Err(Error::Protocol);
        }
        let mut plugin = self.plugin.take().ok_or(Error::Unavailable)?;
        let token = invocation
            .as_ref()
            .map(|c| c.cancel.clone())
            .unwrap_or_else(|| cancel.child_token());
        let mut guard = Cancellation {
            token: token.clone(),
            interrupt: plugin.cancel_handle(),
            armed: true,
        };
        let active = self.active.clone();
        let permit = self.permit.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = (|| {
                if token.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                *active.lock().map_err(|_| Error::Unavailable)? = invocation;
                let raw = plugin
                    .call::<_, &[u8]>(function, input)
                    .map_err(|_| Error::Execution)?;
                if raw.len() > MAX_CHUNK {
                    return Err(Error::Protocol);
                }
                Ok(serde_json::from_slice(raw)?)
            })();
            if let Ok(mut active) = active.lock() {
                *active = None;
            }
            (plugin, result)
        });
        let (plugin, result) = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(timeout, task) => result.map_err(|_| Error::Timeout)?.map_err(|_| Error::Execution)?,
        };
        guard.armed = false;
        if result.is_ok() {
            self.plugin = Some(plugin);
        }
        result
    }
    pub async fn invoke(
        &mut self,
        mut operation: Operation,
        services: Arc<dyn Services>,
        events: mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.package.verify()?;
        let entry = self.package.entry(&self.entrypoint)?;
        if !entry.provides.contains(&operation.extension()) {
            return Err(Error::Unsupported);
        }
        if let Operation::Speech {
            input: SpeechInput::Synthesize { generation, .. },
        } = &mut operation
        {
            *generation = self.generation;
        }
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = events.closed() => return Err(Error::Cancelled),
            result = tokio::time::timeout(EXECUTION_TIMEOUT, services.check()) => result.map_err(|_| Error::Timeout)?.map_err(|_| Error::Denied)?,
        }
        let input = serde_json::to_vec(&operation)?;
        let context = Arc::new(Invocation {
            services,
            declared: entry.uses.clone(),
            calls: AtomicUsize::new(0),
            events,
            validator: Mutex::new(Events::new(&operation)),
            cancel: cancel.child_token(),
            runtime: tokio::runtime::Handle::current(),
        });
        let result = tokio::select! { biased;
            _ = context.events.closed() => Err(Error::Cancelled),
            result = self.call("ene_invoke", input, Some(context.clone()), cancel, EXECUTION_TIMEOUT) => result,
        };
        let result = async {
            let value = result?;
            context.validator.lock().map_err(|_| Error::Unavailable)?.finish()?;
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::time::timeout(EXECUTION_TIMEOUT, context.services.check()) => result.map_err(|_| Error::Timeout)?.map_err(|_| Error::Denied)?,
            }
            Ok(value)
        }.await;
        if result.is_err() {
            self.plugin = None;
        }
        result
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub async fn stop(&mut self) {
        self.plugin = None;
    }
}
#[derive(Clone, Default)]
pub struct Supervisor {
    tracker: tokio_util::task::TaskTracker,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}
impl Supervisor {
    pub fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut tasks = self.tasks.lock().map_err(|_| Error::Unavailable)?;
        if self.tracker.is_closed() {
            return Err(Error::Unavailable);
        }
        tasks.retain(|t| !t.is_finished());
        if tasks.len() >= MAX_INSTANCES {
            return Err(Error::Unavailable);
        }
        tasks.push(self.tracker.spawn(task).abort_handle());
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.tracker.close();
        if tokio::time::timeout(Duration::from_secs(4), self.tracker.wait())
            .await
            .is_err()
        {
            if let Ok(tasks) = self.tasks.lock() {
                for task in tasks.iter() {
                    task.abort();
                }
            }
            self.tracker.wait().await;
        }
    }
}
