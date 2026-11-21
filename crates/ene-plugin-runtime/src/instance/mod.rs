//! Extism instances execute on Tokio's blocking pool; host capabilities stay explicit.
mod invocation;
mod json;
#[cfg(test)]
mod lifecycle_tests;

pub use invocation::{DenyServices, Services};

use crate::{Error, Package, Result, contract::Events, limits::*};
use ene_plugin::*;
use invocation::{Cancellation, Invocation, SLOTS, ene_emit, ene_service};
use json::BoundedJson;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex, Weak, atomic::AtomicUsize},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;

pub struct Instance {
    plugin: Option<extism::Plugin>,
    context: extism::UserData<Weak<Invocation>>,
    package: Package,
    entrypoint: String,
    permit: Arc<OwnedSemaphorePermit>,
    #[cfg(test)]
    native_exit: Option<tokio::sync::oneshot::Sender<lifecycle_tests::NativeExit>>,
    /// What a provider entry declared at `ene_initialize`; guests that are not
    /// providers declare nothing the host reads.
    pub capabilities: Option<LlmCapabilities>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("package", &self.package.manifest.id)
            .field("entrypoint", &self.entrypoint)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}
fn extension_timeout(extension: Extension) -> Duration {
    if extension == Extension::Harness {
        HARNESS_EXECUTION_TIMEOUT
    } else {
        EXECUTION_TIMEOUT
    }
}
fn build(
    package: &Package,
    file: &str,
    timeout: Duration,
    context: extism::UserData<Weak<Invocation>>,
) -> Result<extism::Plugin> {
    let code = package.code(file)?.to_vec();
    let mut manifest = extism::Manifest::new([extism::Wasm::data(code)])
        .disallow_all_hosts()
        .with_memory_max(WASM_MEMORY_PAGES)
        .with_timeout(timeout);
    // Extism vars are not part of the manifest capability; disable writes.
    // `MemoryOptions` is not re-exported by the host SDK, so the one field is
    // set directly rather than adding `extism-manifest` as a second pin.
    manifest.memory.max_var_bytes = Some(0);
    // Extism instantiates the kernel, the linked guest and the callable guest.
    // `max_memory_size` is not the manifest page cap repeated: it sizes the
    // pooling allocator's per-memory virtual reservation, which otherwise
    // defaults to 4 GiB per slot regardless of the runtime growth limiter.
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
            [extism::PTR],
            [extism::PTR],
            context.clone(),
            ene_service,
        )
        .with_function("ene_emit", [extism::PTR], [], context, ene_emit)
        .build()
        .map_err(Error::Execution)
}
impl Instance {
    pub async fn load(
        package: Package,
        entrypoint: String,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::load_for_target(package, entrypoint, Target::Host, cancel).await
    }

    pub async fn load_for_target(
        package: Package,
        entrypoint: String,
        expected_target: Target,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let entry = package.entry(&entrypoint)?;
        if entry.target != expected_target || entry.target == Target::Acp {
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
            let context = extism::UserData::new(Weak::new());
            let plugin = build(&package, &file, timeout, context.clone())?;
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            Ok(Self {
                plugin: Some(plugin),
                context,
                package,
                entrypoint,
                permit: Arc::new(permit),
                #[cfg(test)]
                native_exit: None,
                capabilities: None,
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
    async fn call<T: DeserializeOwned + Send + 'static>(
        &mut self,
        function: &'static str,
        input: Vec<u8>,
        invocation: Option<Arc<Invocation>>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<T> {
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
        let context = self.context.get().map_err(Error::Execution)?;
        #[cfg(test)]
        let native_exit = self.native_exit.take();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = (|| {
                if token.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                *context.lock().map_err(|_| Error::Unavailable)? =
                    invocation.as_ref().map(Arc::downgrade).unwrap_or_default();
                let result = plugin.call::<_, BoundedJson<T, MAX_CHUNK>>(function, input);
                #[cfg(test)]
                if let Some(native_exit) = native_exit {
                    let _ = native_exit.send(lifecycle_tests::NativeExit {
                        error: result.as_ref().err().map(ToString::to_string),
                        fuel_consumed: plugin.fuel_consumed(),
                    });
                }
                *context.lock().map_err(|_| Error::Unavailable)? = Weak::new();
                result.map(|value| value.0).map_err(json::error)
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
        let input = json::encode::<_, MAX_MESSAGE_BYTES>(&operation)?;
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
            result = async {
                match &operation {
                    Operation::Embed { .. } => {
                        let value = self.call::<EmbeddingResult>("ene_invoke", input, Some(context.clone()), cancel, timeout).await?;
                        Ok(serde_json::to_value(value)?)
                    }
                    Operation::Synthesize { .. } => {
                        let value = self.call::<SynthesisResult>("ene_invoke", input, Some(context.clone()), cancel, timeout).await?;
                        Ok(serde_json::to_value(value)?)
                    }
                    Operation::Transcribe { .. } => {
                        let value = self.call::<TranscriptionResult>("ene_invoke", input, Some(context.clone()), cancel, timeout).await?;
                        Ok(serde_json::to_value(value)?)
                    }
                    _ => self.call::<Value>("ene_invoke", input, Some(context.clone()), cancel, timeout).await,
                }
            } => result,
        };
        let plugin = self.plugin.take();
        let result = async {
            let value = result?;
            context.validator.lock().map_err(|_| Error::Unavailable)?.finish()?;
            tokio::select! { biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = context.events.closed() => return Err(Error::Cancelled),
                result = tokio::time::timeout(timeout, context.services.check()) => result.map_err(|_| Error::Timeout)??,
            }
            Ok(value)
        }.await;
        if result.is_ok() {
            self.plugin = plugin;
        }
        result
    }

    /// Whether a plugin is still attached. A cancelled or failed call takes
    /// the plugin out; the owner reloads the instance before the next call.
    pub fn is_usable(&self) -> bool {
        self.plugin.is_some()
    }

    /// Run the guest's initialization hook. A provider entry declares its
    /// capabilities here; they are decoded once and the document is dropped.
    pub async fn initialize(&mut self, cancel: &CancellationToken) -> Result<()> {
        let entry = self.package.entry(&self.entrypoint)?;
        let llm_provider = entry.provider.is_some() && entry.provides.contains(&Extension::Llm);
        if llm_provider {
            let capabilities = self
                .call::<LlmCapabilities>(
                    "ene_initialize",
                    b"{}".to_vec(),
                    None,
                    cancel,
                    INITIALIZE_TIMEOUT,
                )
                .await?;
            if !capabilities.streaming_text || !capabilities.openai_http {
                return Err(Error::Unsupported);
            }
            self.capabilities = Some(capabilities);
        } else {
            self.call::<Value>(
                "ene_initialize",
                b"{}".to_vec(),
                None,
                cancel,
                INITIALIZE_TIMEOUT,
            )
            .await?;
        }
        Ok(())
    }
}
