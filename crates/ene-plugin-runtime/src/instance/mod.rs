//! Extism instances execute on Tokio's blocking pool; host capabilities stay explicit.
mod invocation;
mod supervisor;

pub use invocation::{DenyServices, Services};
pub use supervisor::Supervisor;

use crate::{Error, Package, Result, contract::Events, limits::*};
use ene_plugin::*;
use invocation::{Cancellation, Invocation, SLOTS, ene_emit, ene_service};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex, atomic::AtomicUsize},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;

pub struct Instance {
    plugin: Option<extism::Plugin>,
    package: Package,
    entrypoint: String,
    generation: u64,
    permit: Arc<OwnedSemaphorePermit>,
    pub capabilities: Value,
}
fn extension_timeout(extension: Extension) -> Duration {
    if extension == Extension::Harness {
        HARNESS_EXECUTION_TIMEOUT
    } else {
        EXECUTION_TIMEOUT
    }
}
fn build(package: &Package, file: &str, timeout: Duration) -> Result<extism::Plugin> {
    let code = package.code(file)?.to_vec();
    let mut manifest = extism::Manifest::new([extism::Wasm::data(code)])
        .with_allowed_hosts(std::iter::empty())
        .with_memory_max(WASM_MEMORY_PAGES)
        .with_timeout(timeout);
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
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let entry = package.entry(&entrypoint)?;
        if entry.target != Target::Host {
            return Err(Error::Unsupported);
        }
        let file = entry.file.clone();
        // A harness entrypoint can run a whole task segment; other extensions
        // keep the short call deadline.
        let timeout = extension_timeout(
            entry
                .provides
                .iter()
                .copied()
                .find(|extension| *extension == Extension::Harness)
                .unwrap_or(Extension::Llm),
        );
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
            let plugin = build(&package, &file, timeout)?;
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
                let raw = match invocation {
                    Some(invocation) => {
                        plugin.call_with_host_context::<_, &[u8], _>(function, input, invocation)
                    }
                    None => plugin.call::<_, &[u8]>(function, input),
                }
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
        operation: Operation,
        services: Arc<dyn Services>,
        events: mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let entry = self.package.entry(&self.entrypoint)?;
        if !entry.provides.contains(&operation.extension()) {
            return Err(Error::Unsupported);
        }
        let timeout = extension_timeout(operation.extension());
        tokio::select! { biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = events.closed() => return Err(Error::Cancelled),
            result = tokio::time::timeout(timeout, services.check()) => result.map_err(|_| Error::Timeout)??,
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
            timeout,
            runtime: tokio::runtime::Handle::current(),
        });
        let result = tokio::select! { biased;
            _ = context.events.closed() => Err(Error::Cancelled),
            result = self.call("ene_invoke", input, Some(context.clone()), cancel, timeout) => result,
        };
        let result = async {
            let value = result?;
            context.validator.lock().map_err(|_| Error::Unavailable)?.finish()?;
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::time::timeout(timeout, context.services.check()) => result.map_err(|_| Error::Timeout)??,
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
