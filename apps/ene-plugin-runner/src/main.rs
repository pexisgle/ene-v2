//! Control IO stays on Tokio while guest execution runs on a separate thread.
use anyhow::{Context, Result, bail};
use clap::Parser;
use ene_ipc::plugin::*;
use ene_plugin_runtime::Package;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    package: Option<PathBuf>,
    #[arg(long)]
    entrypoint: Option<String>,
    #[arg(long)]
    validate: bool,
    #[arg(long)]
    schema: bool,
}
type ServiceReply = std::result::Result<Value, Failure>;
struct Bridge {
    call: Mutex<Option<Call>>,
    sequence: AtomicU64,
    ids: AtomicU64,
    pending: Mutex<BTreeMap<String, oneshot::Sender<ServiceReply>>>,
    output: mpsc::Sender<WorkerMessage>,
    cancelled: AtomicBool,
    interrupt: Mutex<Option<extism::CancelHandle>>,
    deadline: Mutex<Instant>,
}
impl Bridge {
    fn call(&self) -> Result<Call> {
        if self.cancelled.load(Ordering::Acquire) {
            bail!("cancelled");
        }
        self.call
            .lock()
            .unwrap()
            .clone()
            .context("no active invocation")
    }
    async fn service(&self, request: ServiceRequest) -> Result<Value> {
        let call = self.call()?;
        let id = self.ids.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= 16 || self.cancelled.load(Ordering::Acquire) {
                bail!("cancelled or too many pending calls");
            }
            pending.insert(id.clone(), tx);
        }
        self.output
            .send(WorkerMessage::Service {
                call,
                service_id: id,
                request,
            })
            .await?;
        rx.await?.map_err(|failure| anyhow::anyhow!(failure.code))
    }
    async fn emit(&self, event: Event) -> Result<()> {
        if serde_json::to_vec(&event)?.len() > MAX_CHUNK {
            bail!("event too large");
        }
        let call = self.call()?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        if sequence >= 65536 {
            bail!("event limit");
        }
        self.output
            .send(WorkerMessage::Event {
                call,
                sequence,
                event,
            })
            .await?;
        Ok(())
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        for (_, reply) in std::mem::take(&mut *self.pending.lock().unwrap()) {
            let _ = reply.send(Err(Failure::new("cancelled")));
        }
        if let Some(handle) = &*self.interrupt.lock().unwrap() {
            let _ = handle.cancel();
        }
    }
}
enum Engine {
    Wasm(Box<extism::Plugin>),
    Luau {
        lua: mlua::Lua,
        module: mlua::Table,
        scheduler: mlua_luau_scheduler::Scheduler,
    },
}
impl Engine {
    fn load(
        package: &Package,
        entrypoint: &str,
        bridge: Arc<Bridge>,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<Self> {
        let entry = package.entry(entrypoint)?;
        match entry.runtime {
            Runtime::Extism => {
                let manifest = extism::Manifest::new([extism::Wasm::data(
                    package.code(&entry.file)?.to_vec(),
                )])
                .with_memory_max(1024)
                .with_timeout(Duration::from_secs(180));
                let service = bridge.clone();
                let handle = runtime.handle().clone();
                let emit = bridge.clone();
                let emit_handle = runtime.handle().clone();
                let mut builder = extism::PluginBuilder::new(manifest).with_wasi(false);
                if cfg!(windows) {
                    // Wasmtime's known-folder cache is outside this worker's capsule.
                    builder = builder.with_cache_disabled();
                }
                let plugin = builder
                    .with_fuel_limit(100_000_000)
                    .with_initialization_fuel_limit(10_000_000)
                    .with_function(
                        "ene_service",
                        [extism::ValType::I64],
                        [extism::ValType::I64],
                        extism::UserData::new(()),
                        move |plugin, input, output, _| {
                            let raw: String = plugin.memory_get_val(&input[0])?;
                            if raw.len() > MAX_CHUNK {
                                bail!("service request too large");
                            }
                            let result = handle
                                .block_on(service.service(serde_json::from_str(&raw)?))
                                .map_err(|_| Failure::new("service_failed"));
                            plugin.memory_set_val(&mut output[0], serde_json::to_string(&result)?)
                        },
                    )
                    .with_function(
                        "ene_emit",
                        [extism::ValType::I64],
                        [],
                        extism::UserData::new(()),
                        move |plugin, input, _, _| {
                            let raw: String = plugin.memory_get_val(&input[0])?;
                            if raw.len() > MAX_CHUNK {
                                bail!("event too large");
                            }
                            emit_handle.block_on(emit.emit(serde_json::from_str(&raw)?))
                        },
                    )
                    .build()?;
                *bridge.interrupt.lock().unwrap() = Some(plugin.cancel_handle());
                Ok(Self::Wasm(Box::new(plugin)))
            }
            Runtime::Luau => {
                use mlua::LuaSerdeExt;
                let lua = mlua::Lua::new();
                lua.set_memory_limit(64 * 1024 * 1024)?;
                let control = bridge.clone();
                lua.set_interrupt(move |_| {
                    if control.cancelled.load(Ordering::Acquire)
                        || Instant::now() >= *control.deadline.lock().unwrap()
                    {
                        Err(mlua::Error::RuntimeError(
                            "cancelled or execution deadline exceeded".into(),
                        ))
                    } else {
                        Ok(mlua::VmState::Continue)
                    }
                });
                let sdk = lua.create_table()?;
                let service = bridge.clone();
                sdk.set(
                    "service",
                    lua.create_async_function(move |lua, value: mlua::Value| {
                        let bridge = service.clone();
                        async move {
                            let request: ServiceRequest = lua.from_value(value)?;
                            let value = bridge
                                .service(request)
                                .await
                                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                            lua.to_value(&value)
                        }
                    })?,
                )?;
                sdk.set(
                    "emit",
                    lua.create_async_function(move |lua, value: mlua::Value| {
                        let bridge = bridge.clone();
                        async move {
                            let event = lua.from_value(value)?;
                            bridge
                                .emit(event)
                                .await
                                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
                        }
                    })?,
                )?;
                lua.globals().set("ene", sdk)?;
                lua.globals().set("print", mlua::Value::Nil)?;
                let files = package.clone();
                let cache = lua.create_table()?;
                lua.globals().set(
                    "require",
                    lua.create_function(move |lua, path: String| {
                        if !ene_plugin_runtime::package::relative(&path) {
                            return Err(mlua::Error::RuntimeError("invalid module".into()));
                        }
                        let cached: mlua::Value = cache.get(path.as_str())?;
                        if !cached.is_nil() {
                            return Ok(cached);
                        }
                        let code = files
                            .code(&path)
                            .map_err(|_| mlua::Error::RuntimeError("unknown module".into()))?;
                        let result: mlua::Value = lua.load(code).set_name(&path).eval()?;
                        cache.set(path, result.clone())?;
                        Ok(result)
                    })?,
                )?;
                lua.sandbox(true)?;
                let module: mlua::Table = lua
                    .load(package.code(&entry.file)?)
                    .set_name(&entry.file)
                    .eval()?;
                let scheduler = mlua_luau_scheduler::Scheduler::new(lua.clone());
                scheduler.remove_error_callback();
                Ok(Self::Luau {
                    lua,
                    module,
                    scheduler,
                })
            }
            _ => bail!("unsupported runner runtime"),
        }
    }
    fn initialize(&mut self) -> Result<Value> {
        match self {
            Self::Wasm(plugin) => Ok(serde_json::from_str(
                plugin.call::<_, &str>("ene_initialize", "{}")?,
            )?),
            Self::Luau { lua, module, .. } => {
                use mlua::LuaSerdeExt;
                let function: mlua::Function = module.get("initialize")?;
                Ok(lua.from_value(function.call::<mlua::Value>(())?)?)
            }
        }
    }
    fn invoke(&mut self, operation: Operation, runtime: &tokio::runtime::Runtime) -> Result<Value> {
        match self {
            Self::Wasm(plugin) => Ok(serde_json::from_str(
                plugin.call::<_, &str>("ene_invoke", serde_json::to_vec(&operation)?)?,
            )?),
            Self::Luau {
                lua,
                module,
                scheduler,
            } => {
                use mlua::LuaSerdeExt;
                let function: mlua::Function = module.get("invoke")?;
                let id = scheduler.push_thread_front(function, lua.to_value(&operation)?)?;
                runtime.block_on(scheduler.run());
                let result = scheduler
                    .get_thread_result(id)
                    .context("missing coroutine result")??;
                let value = result.front().cloned().unwrap_or(mlua::Value::Nil);
                Ok(lua.from_value(value)?)
            }
        }
    }
}
fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(main_async());
    // Tokio stdin uses a blocking reader. It must not keep a cancelled worker alive.
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}
async fn main_async() -> Result<()> {
    let args = Args::parse();
    if args.schema {
        println!("{}", serde_json::to_string_pretty(&schemas())?);
        return Ok(());
    }
    let root = args.package.context("--package is required")?;
    if args.validate {
        let package = Package::load(&root)?;
        println!("{}", package.revision);
        return Ok(());
    }
    let package = Package::load_snapshot(&root)?;
    let entrypoint = args.entrypoint.context("--entrypoint is required")?;
    run(package, entrypoint).await
}
async fn run(package: Package, entrypoint: String) -> Result<()> {
    let (mut input, mut output) =
        ene_plugin_runtime::transport::worker(tokio::io::stdin(), tokio::io::stdout()).await?;
    let Some(HostMessage::Initialize {
        protocol_version: PROTOCOL_VERSION,
        call,
        entrypoint: requested,
    }) = input.receive().await?
    else {
        bail!("expected initialize");
    };
    if requested != entrypoint {
        bail!("entrypoint mismatch");
    }
    let (tx, mut rx) = mpsc::channel(16);
    let bridge = Arc::new(Bridge {
        call: Mutex::new(None),
        sequence: AtomicU64::new(0),
        ids: AtomicU64::new(0),
        pending: Mutex::new(BTreeMap::new()),
        output: tx,
        cancelled: AtomicBool::new(false),
        interrupt: Mutex::new(None),
        deadline: Mutex::new(Instant::now() + Duration::from_secs(30)),
    });
    let (commands, mut command_rx) = mpsc::channel::<(Call, Operation)>(1);
    let (done_tx, mut done_rx) = mpsc::channel(1);
    let execution = bridge.clone();
    let generation = call.generation;
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let mut engine = Engine::load(&package, &entrypoint, execution.clone(), &runtime)?;
            let capabilities = engine.initialize()?;
            execution.output.blocking_send(WorkerMessage::Ready {
                protocol_version: PROTOCOL_VERSION,
                call,
                capabilities,
            })?;
            while let Some((call, operation)) = command_rx.blocking_recv() {
                *execution.deadline.lock().unwrap() = Instant::now() + Duration::from_secs(180);
                let result = if execution.cancelled.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("cancelled"))
                } else {
                    engine.invoke(operation, &runtime)
                };
                let result = if execution.cancelled.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("cancelled"))
                } else {
                    result
                };
                let message = match result {
                    Ok(result) => WorkerMessage::Complete { call, result },
                    Err(_) => WorkerMessage::Failed {
                        call,
                        error: Failure::new("execution_failed"),
                    },
                };
                execution.output.blocking_send(message)?;
                if execution.cancelled.load(Ordering::Acquire) {
                    break;
                }
            }
            Ok(())
        })();
        let _ = done_tx.blocking_send(result);
    });
    let writer_stop = tokio_util::sync::CancellationToken::new();
    let stop = writer_stop.clone();
    let writer_bridge = bridge.clone();
    let mut writer = tokio::spawn(async move {
        loop {
            tokio::select! {biased;
                message=rx.recv()=>{
                    let Some(message)=message else {return Ok::<_,anyhow::Error>(());};
                    if let WorkerMessage::Complete {call,..}|WorkerMessage::Failed {call,..}=&message {
                        let mut current=writer_bridge.call.lock().unwrap();
                        if current.as_ref()==Some(call) {*current=None;}
                    }
                    tokio::time::timeout(Duration::from_secs(3),output.send(&message)).await??;
                }
                _=stop.cancelled()=>return Ok(()),
            }
        }
    });
    let mut execution_finished = false;
    let result = async {
        loop {
            tokio::select! {
                finished = done_rx.recv() => {
                    execution_finished = true;
                    return finished.context("execution thread failed")?;
                }
                result=&mut writer=>{return result?;}
                message = input.receive() => {
                    match message? {
                        Some(HostMessage::Invoke {call, operation}) if bridge.call.lock().unwrap().is_none() && call.generation == generation && !call.request_id.is_empty() => {
                            bridge.sequence.store(0,Ordering::Relaxed);
                            *bridge.call.lock().unwrap() = Some(call.clone());
                            commands.try_send((call,operation))?;
                        }
                        Some(HostMessage::ServiceResult {call,service_id,result}) if bridge.call.lock().unwrap().as_ref() == Some(&call) => {
                            let reply = bridge.pending.lock().unwrap().remove(&service_id).context("unknown service reply")?;
                            let _ = reply.send(result);
                        }
                        Some(HostMessage::Cancel {call}) if bridge.call.lock().unwrap().as_ref() == Some(&call) => { bridge.cancel(); }
                        Some(HostMessage::Shutdown) | None => { return Ok(()); }
                        _ => bail!("invalid control sequence"),
                    }
                }
            }
        }
    }.await;
    bridge.cancel();
    drop(commands);
    writer_stop.cancel();
    if !writer.is_finished() {
        let _ = writer.await;
    }
    if execution_finished && result.is_ok() {
        // Sending finishes before the remote channel necessarily decodes its
        // terminal event. Keep Remoc alive until the host acknowledges with
        // Shutdown; a disconnected or unresponsive host cannot keep us alive.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match input.receive().await {
                    Ok(Some(HostMessage::Shutdown)) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
        })
        .await;
    }
    // Host supervisor enforces the final deadline even for a non-cooperative native runtime.
    result
}
