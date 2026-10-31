//! Extism instances execute on Tokio's blocking pool; host capabilities stay explicit.
use crate::{Error, Package, Result, contract::Events, limits::*};
use ene_plugin::*;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    cell::RefCell,
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
thread_local! {
    static ACTIVE: RefCell<Option<Arc<Invocation>>> = const { RefCell::new(None) };
}
fn active() -> Result<Arc<Invocation>> {
    ACTIVE
        .with(|slot| slot.borrow().clone())
        .ok_or(Error::Denied)
}
struct ActiveGuard(Option<Arc<Invocation>>);
impl ActiveGuard {
    fn set(value: Option<Arc<Invocation>>) -> Self {
        Self(ACTIVE.with(|slot| slot.replace(value)))
    }
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
fn ene_service(
    plugin: &mut extism::CurrentPlugin,
    input: &[extism::Val],
    output: &mut [extism::Val],
    _: extism::UserData<()>,
) -> std::result::Result<(), extism::Error> {
    let raw: &[u8] = plugin.memory_get_val(&input[0])?;
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Protocol.into());
    }
    let invocation = active()?;
    // Charge the bytes actually received and sent, not their re-encoded form.
    invocation.charge(&invocation.bytes, raw.len(), MAX_SERVICE_BYTES)?;
    let request = serde_json::from_slice(raw)?;
    let result = invocation.service(request)?;
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_CHUNK {
        return Err(Error::Protocol.into());
    }
    invocation.charge(&invocation.bytes, bytes.len(), MAX_SERVICE_BYTES)?;
    plugin.memory_set_val(&mut output[0], bytes)
}
fn ene_emit(
    plugin: &mut extism::CurrentPlugin,
    input: &[extism::Val],
    _: &mut [extism::Val],
    _: extism::UserData<()>,
) -> std::result::Result<(), extism::Error> {
    let raw: &[u8] = plugin.memory_get_val(&input[0])?;
    if raw.len() > MAX_CHUNK {
        return Err(Error::Protocol.into());
    }
    let invocation = active()?;
    invocation.charge(&invocation.stream_bytes, raw.len(), MAX_STREAM_BYTES)?;
    invocation.emit(serde_json::from_slice(raw)?)?;
    Ok(())
}
struct Invocation {
    services: Arc<dyn Services>,
    declared: Vec<Service>,
    calls: AtomicUsize,
    bytes: AtomicUsize,
    stream_bytes: AtomicUsize,
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
    package: Package,
    entrypoint: String,
    generation: u64,
    permit: Arc<OwnedSemaphorePermit>,
    pub capabilities: Value,
}
fn build(package: &Package, file: &str) -> Result<extism::Plugin> {
    let code = package.code(file)?.to_vec();
    let mut manifest = extism::Manifest::new([extism::Wasm::data(code)])
        .with_allowed_hosts(std::iter::empty())
        .with_memory_max(WASM_MEMORY_PAGES)
        .with_timeout(EXECUTION_TIMEOUT);
    // Extism vars are not part of the manifest capability; disable writes.
    manifest.memory.max_var_bytes = Some(0);
    // Extism instantiates the kernel, the linked guest and the callable guest.
    let mut allocation = wasmtime::PoolingAllocationConfig::default();
    allocation
        .total_core_instances(3)
        .total_memories(4) // The store's GC heap also occupies a memory slot.
        .total_tables(3)
        .total_gc_heaps(1)
        .max_tables_per_module(1)
        .table_elements(WASM_TABLE_ELEMENTS)
        .max_memory_size(WASM_MEMORY_PAGES as usize * 65536);
    let mut config = wasmtime::Config::new();
    config.allocation_strategy(allocation);
    extism::PluginBuilder::new(manifest)
        .with_wasmtime_config(config)
        .with_wasi(false)
        .with_fuel_limit(WASM_FUEL)
        .with_initialization_fuel_limit(WASM_INITIALIZATION_FUEL)
        .with_function(
            "ene_service",
            [extism::ValType::I64],
            [extism::ValType::I64],
            extism::UserData::new(()),
            ene_service,
        )
        .with_function(
            "ene_emit",
            [extism::ValType::I64],
            [],
            extism::UserData::new(()),
            ene_emit,
        )
        .build()
        .map_err(Error::Execution)
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
        let entry = package.entry(&entrypoint)?;
        if entry.target != target {
            return Err(Error::Unsupported);
        }
        let file = entry.file.clone();
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
            package.verify()?;
            let plugin = build(&package, &file)?;
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            Ok(Self {
                plugin: Some(plugin),
                package,
                entrypoint,
                generation,
                permit: Arc::new(permit),
                capabilities: Value::Null,
            })
        });
        let mut instance = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, build) => result.map_err(|_| Error::Timeout)?.map_err(|e| Error::Execution(e.into()))??,
        };
        drop(stop);
        instance.initialize(cancel).await?;
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
        let permit = self.permit.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = (|| {
                if token.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let _active = ActiveGuard::set(invocation);
                let raw = plugin
                    .call::<_, &[u8]>(function, input)
                    .map_err(Error::Execution)?;
                if raw.len() > MAX_CHUNK {
                    return Err(Error::Protocol);
                }
                Ok(serde_json::from_slice(raw)?)
            })();
            (plugin, result)
        });
        let (plugin, result) = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(timeout, task) => result.map_err(|_| Error::Timeout)?.map_err(|e| Error::Execution(e.into()))?,
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
            result = tokio::time::timeout(EXECUTION_TIMEOUT, services.check()) => result.map_err(|_| Error::Timeout)??,
        }
        let input = serde_json::to_vec(&operation)?;
        let context = Arc::new(Invocation {
            services,
            declared: entry.uses.clone(),
            calls: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            stream_bytes: AtomicUsize::new(0),
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
                result = tokio::time::timeout(EXECUTION_TIMEOUT, context.services.check()) => result.map_err(|_| Error::Timeout)??,
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
    /// Run the guest's initialization hook and validate its declared capabilities.
    pub async fn initialize(&mut self, cancel: &CancellationToken) -> Result<()> {
        self.capabilities = self
            .call(
                "ene_initialize",
                b"{}".to_vec(),
                None,
                cancel,
                INITIALIZE_TIMEOUT,
            )
            .await?;
        if self
            .package
            .entry(&self.entrypoint)?
            .provides
            .contains(&Extension::Llm)
        {
            let capabilities: LlmCapabilities = serde_json::from_value(self.capabilities.clone())?;
            if !capabilities.streaming_text {
                return Err(Error::Unsupported);
            }
        }
        Ok(())
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
