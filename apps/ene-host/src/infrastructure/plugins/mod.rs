//! Host integration: ownership and permissions belong to ene, execution to the runner.
mod harness;
mod provider;
use super::{
    llm::plugin::manifest::{self, Bundle},
    sqlite::Store,
};
use crate::{
    domain::{Error, ErrorKind},
    ports::{Result, plugin::Plugins},
};
use ene_plugin::{CharacterAction, Event, Failure, Operation, ServiceRequest};
use ene_plugin::{Extension, Target};
use ene_plugin_runtime::{
    Package,
    limits::{MAX_EVENTS, MAX_MESSAGE_BYTES},
    worker::{Services, Supervisor, Worker},
};
use ene_rpc::{
    EnablePluginRequest, Plugin, PluginBinding, PluginEntry, PluginInvocation, PluginResult,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
struct Entry {
    package: Package,
    state: String,
    error: Option<String>,
    cancellation: CancellationToken,
    generation: u64,
    capabilities: BTreeMap<String, Value>,
}
type SharedWorker = Arc<Mutex<Option<Worker>>>;
type Sessions = Arc<std::sync::Mutex<BTreeMap<String, SharedWorker>>>;
type Registration = (Manager, String, SharedWorker);
struct Session<'a> {
    guard: tokio::sync::MutexGuard<'a, Option<Worker>>,
    retain: bool,
    registration: Option<Registration>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl std::ops::Deref for Session<'_> {
    type Target = Option<Worker>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}
impl std::ops::DerefMut for Session<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        if !self.retain {
            self.guard.take();
            if let Some((manager, key, worker)) = &self.registration {
                let mut sessions = manager.sessions.lock().unwrap();
                if sessions
                    .get(key)
                    .is_some_and(|current| Arc::ptr_eq(current, worker))
                {
                    sessions.remove(key);
                    manager.binding_tokens.lock().unwrap().remove(key);
                }
            }
        }
    }
}
struct ActiveInvocation {
    manager: Manager,
    key: String,
    token: CancellationToken,
    id: uuid::Uuid,
}
impl ActiveInvocation {
    fn new(manager: Manager, key: String, token: CancellationToken) -> Self {
        let id = uuid::Uuid::new_v4();
        manager
            .invocation_tokens
            .lock()
            .unwrap()
            .insert(key.clone(), (id, token.clone()));
        Self {
            manager,
            key,
            token,
            id,
        }
    }
}
impl Drop for ActiveInvocation {
    fn drop(&mut self) {
        // This guard is dropped before releasing the worker lock.
        let mut tokens = self.manager.invocation_tokens.lock().unwrap();
        if tokens.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            tokens.remove(&self.key);
        }
        self.token.cancel();
    }
}
#[derive(Clone)]
pub struct Manager {
    root: PathBuf,
    cache: PathBuf,
    runner: PathBuf,
    store: Store,
    entries: Arc<RwLock<BTreeMap<String, Entry>>>,
    sessions: Sessions,
    generations: Arc<AtomicU64>,
    pub(crate) supervisor: Supervisor,
    stopped: CancellationToken,
    discovery_error: Arc<RwLock<Option<String>>>,
    binding_tokens: Arc<std::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
    invocation_tokens: Arc<std::sync::Mutex<BTreeMap<String, (uuid::Uuid, CancellationToken)>>>,
    invocation_slots: Arc<tokio::sync::Semaphore>,
}
pub(crate) fn failure(_: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::ExternalFailure)
}
fn conflict() -> Error {
    Error::new(ErrorKind::StateConflict)
}
fn unsupported() -> Error {
    Error::new(ErrorKind::UnsupportedCapability)
}
impl Manager {
    pub fn discover(store: Store) -> Self {
        let data = ene_platform::paths::data();
        let root = std::env::var_os("ENE_PLUGIN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("plugins"));
        let cache = std::env::var_os("ENE_PLUGIN_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("plugin-code"));
        let runner = std::env::var_os("ENE_PLUGIN_RUNNER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| ene_platform::sibling_executable("ene-plugin-runner"));
        let manager = Self {
            root,
            cache,
            runner,
            store,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            sessions: Arc::default(),
            generations: Arc::new(AtomicU64::new(1)),
            supervisor: Supervisor::default(),
            stopped: CancellationToken::new(),
            discovery_error: Arc::default(),
            binding_tokens: Arc::default(),
            invocation_tokens: Arc::default(),
            invocation_slots: Arc::new(tokio::sync::Semaphore::new(16)),
        };
        // Malformed packages are reported by reload; no code is executed during discovery.
        if manager.scan().is_err() {
            *manager.discovery_error.write().unwrap() =
                Some("invalid_manifest_or_duplicate_id".into());
        }
        manager
    }
    fn scan(&self) -> Result<()> {
        let old = self.entries.read().map_err(failure)?.clone();
        let mut next = BTreeMap::new();
        let mut provider_ids = std::collections::BTreeSet::new();
        if self.root.exists() {
            for path in std::fs::read_dir(&self.root).map_err(failure)? {
                let package = Package::load(&path.map_err(failure)?.path()).map_err(failure)?;
                for entry in &package.manifest.entrypoints {
                    if let Some(provider) = &entry.provider {
                        manifest::validate(provider)?;
                        if !provider_ids.insert(provider.id.clone()) {
                            return Err(conflict());
                        }
                    }
                }
                let id = package.manifest.id.clone();
                let entry = old
                    .get(&id)
                    .filter(|e| e.package.revision == package.revision)
                    .cloned()
                    .unwrap_or_else(|| Entry {
                        package,
                        state: "disabled".into(),
                        error: None,
                        cancellation: CancellationToken::new(),
                        generation: self.generations.fetch_add(1, Ordering::Relaxed),
                        capabilities: BTreeMap::new(),
                    });
                if next.insert(id, entry).is_some() {
                    return Err(conflict());
                }
            }
        }
        let mut entries = self.entries.write().map_err(failure)?;
        for (id, entry) in entries.iter() {
            if !next
                .get(id)
                .is_some_and(|e| e.generation == entry.generation)
            {
                entry.cancellation.cancel();
            }
        }
        *entries = next;
        Ok(())
    }
    fn ready(&self, id: &str, revision: Option<&str>) -> Result<Entry> {
        if self.stopped.is_cancelled() {
            return Err(Error::unavailable());
        }
        let e = self
            .entries
            .read()
            .map_err(failure)?
            .get(id)
            .cloned()
            .ok_or_else(unsupported)?;
        if e.state != "ready" || e.cancellation.is_cancelled() {
            return Err(Error::unavailable());
        }
        if revision.is_some_and(|r| e.package.revision != r) {
            return Err(conflict());
        }
        e.package.verify().map_err(|_| conflict())?;
        Ok(e)
    }
    pub(crate) fn provider_entry(&self, id: &str) -> Option<(String, String)> {
        self.entries.read().ok()?.iter().find_map(|(package, e)| {
            e.package
                .manifest
                .entrypoints
                .iter()
                .find(|entry| {
                    entry.target == Target::Host
                        && entry.provider.as_ref().is_some_and(|p| p.id == id)
                })
                .map(|entry| (package.clone(), entry.id.clone()))
        })
    }
    pub(crate) fn provider_bundle(&self, id: &str) -> Result<Bundle> {
        let (package, entrypoint) = self.provider_entry(id).ok_or_else(unsupported)?;
        let entry = self.ready(&package, None)?;
        bundle(&entry.package, &entrypoint)
    }
    pub(crate) fn descriptors(&self) -> Vec<crate::llm::Descriptor> {
        self.entries
            .read()
            .map(|entries| {
                entries
                    .values()
                    .flat_map(|e| {
                        e.package.manifest.entrypoints.iter().filter_map(|entry| {
                            if entry.target != Target::Host {
                                return None;
                            }
                            let b = bundle(&e.package, &entry.id).ok()?;
                            let state = if e.package.verify().is_ok() {
                                &e.state
                            } else {
                                "changed"
                            };
                            let capabilities = e
                                .capabilities
                                .get(&entry.id)
                                .cloned()
                                .and_then(|c| serde_json::from_value(c).ok())
                                .unwrap_or_default();
                            Some(b.descriptor(state, capabilities, e.error.clone()))
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    async fn stop_sessions(&self, package: Option<&str>) {
        let keys = {
            let mut tokens = self.binding_tokens.lock().unwrap();
            let keys: Vec<String> = tokens
                .iter()
                .filter(|(_, (id, _))| package.is_none_or(|p| p == id))
                .map(|(key, _)| key.clone())
                .collect();
            for key in &keys {
                if let Some((_, token)) = tokens.remove(key) {
                    token.cancel();
                }
            }
            keys
        };
        let workers = {
            let mut sessions = self.sessions.lock().unwrap();
            keys.iter()
                .filter_map(|key| sessions.remove(key))
                .collect::<Vec<_>>()
        };
        for worker in workers {
            if let Some(mut worker) = worker.lock().await.take() {
                worker.stop().await;
            }
        }
        for key in keys {
            if let Ok(mut journal) = self.store.plugin_get("harness".into(), key.clone()).await
                && journal.is_object()
                && !matches!(
                    journal["status"].as_str(),
                    Some("completed" | "failed" | "cancelled")
                )
            {
                journal["status"] = json!("failed");
                journal["report"] = json!("plugin_stopped");
                journal["pending"] = Value::Null;
                let _ = self.store.plugin_set("harness".into(), key, journal).await;
            }
        }
    }
    async fn change_enabled(&self, id: String, input: EnablePluginRequest) -> Result<()> {
        let (package, generation, cancel) = {
            let mut entries = self.entries.write().map_err(failure)?;
            let e = entries.get_mut(&id).ok_or_else(unsupported)?;
            if e.state == "unavailable" {
                return Err(unsupported());
            }
            if e.package.revision != input.revision {
                return Err(conflict());
            }
            e.package.verify().map_err(failure)?;
            e.cancellation.cancel();
            e.cancellation = self.stopped.child_token();
            e.generation = self.generations.fetch_add(1, Ordering::Relaxed);
            e.state = if input.enabled {
                "initializing"
            } else {
                "disabled"
            }
            .into();
            e.capabilities.clear();
            (e.package.clone(), e.generation, e.cancellation.clone())
        };
        // Cancelling old generations releases service waits before collecting workers.
        self.stop_sessions(Some(&id)).await;
        let result = async {
            let mut capabilities = BTreeMap::new();
            if input.enabled {
                for entry in package
                    .manifest
                    .entrypoints
                    .iter()
                    .filter(|e| e.target == Target::Host)
                {
                    let installed = package.install(&self.cache).map_err(failure)?;
                    let mut worker = Worker::start(
                        installed,
                        entry.id.clone(),
                        &self.runner,
                        Target::Host,
                        generation,
                        &cancel,
                    )
                    .await
                    .map_err(failure)?;
                    capabilities.insert(entry.id.clone(), worker.capabilities.clone());
                    worker.stop().await;
                }
            }
            Ok::<_, Error>(capabilities)
        }
        .await;
        {
            let mut entries = self.entries.write().map_err(failure)?;
            let e = entries.get_mut(&id).ok_or_else(unsupported)?;
            if e.generation != generation {
                return Err(conflict());
            }
            match &result {
                Ok(c) => {
                    e.state = if input.enabled { "ready" } else { "disabled" }.into();
                    e.capabilities = c.clone();
                    e.error = None;
                }
                Err(_) => {
                    e.state = "failed".into();
                    e.error = Some("隔離環境の起動または初期化に失敗しました".into());
                    e.cancellation.cancel();
                }
            }
        }
        result?;
        self.store
            .plugin_set("enabled".into(), id.clone(), json!({"id":id,"input":input}))
            .await
            .map_err(failure)?;
        Ok(())
    }
    async fn invoke_owned(&self, input: PluginInvocation) -> Result<PluginResult> {
        let binding: PluginBinding = serde_json::from_value(
            self.store
                .plugin_get("bindings".into(), input.binding_id)
                .await
                .map_err(failure)?,
        )
        .map_err(failure)?;
        let entry = self.ready(&binding.plugin_id, Some(&binding.revision))?;
        let operation: Operation =
            serde_json::from_str(&input.operation.json).map_err(|_| Error::invalid())?;
        if matches!(
            operation,
            Operation::Generate { .. } | Operation::ListModels { .. }
        ) {
            return Err(unsupported());
        }
        if let Operation::Character { target, event } = &operation {
            if target.device_id != binding.device_id
                || target.character_id != binding.companion_id
                || !ene_plugin_runtime::package::identifier(&target.device_id)
            {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            if matches!(event, ene_plugin::CharacterEvent::Window { .. }) {
                return Err(unsupported());
            }
        }
        if let Operation::Character {
            event: ene_plugin::CharacterEvent::Custom { name, .. },
            ..
        } = &operation
            && (!name.starts_with(&format!("{}:", binding.plugin_id)) || name.len() > 160)
        {
            return Err(Error::invalid());
        }
        let persistent = matches!(
            operation,
            Operation::Character { .. } | Operation::Harness { .. }
        );
        // Cancel reaches the active invocation before waiting for its worker lock.
        let cancelling = matches!(
            &operation,
            Operation::Harness {
                input: ene_plugin::HarnessInput::Cancel
            }
        );
        if cancelling
            && let Some((_, token)) = self.invocation_tokens.lock().unwrap().get(&binding.id)
        {
            token.cancel();
        }
        let permit = if cancelling {
            None
        } else {
            Some(
                self.invocation_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::unavailable())?,
            )
        };
        let key = if persistent {
            binding.id.clone()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let (worker, lifetime) = if persistent {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= 16 && !sessions.contains_key(&key) {
                return Err(Error::unavailable());
            }
            let worker = sessions.entry(key.clone()).or_default().clone();
            let lifetime = self
                .binding_tokens
                .lock()
                .unwrap()
                .entry(key.clone())
                .or_insert_with(|| (binding.plugin_id.clone(), entry.cancellation.child_token()))
                .1
                .clone();
            (worker, lifetime)
        } else {
            (Arc::new(Mutex::new(None)), entry.cancellation.child_token())
        };
        let mut session = Session {
            guard: worker.lock().await,
            retain: false,
            registration: persistent.then(|| (self.clone(), key.clone(), worker.clone())),
            _permit: permit,
        };
        if persistent
            && !cancelling
            && !self
                .sessions
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &worker))
        {
            return Err(conflict());
        }
        if cancelling {
            let journal = self
                .harness_begin(&binding, &ene_plugin::HarnessInput::Cancel)
                .await?;
            let events = vec![
                serde_json::to_value(Event::Harness {
                    event: ene_plugin::HarnessEvent::Cancelled,
                })
                .map_err(failure)?,
            ];
            let task_id = self.harness_cancel(&binding, journal, &events).await?;
            if let Some(mut worker) = session.take() {
                worker.stop().await;
            }
            return Ok(PluginResult {
                generation: entry.generation,
                events: events.into_iter().map(document).collect(),
                result: document(json!({"task_id": task_id})).into(),
                ..Default::default()
            });
        }
        let cancel = lifetime.child_token();
        let _invocation = ActiveInvocation::new(self.clone(), key.clone(), cancel.clone());
        let journal = if let Operation::Harness { input } = &operation {
            Some(self.harness_begin(&binding, input).await?)
        } else {
            None
        };
        if let Some(journal) = &journal {
            self.store
                .plugin_set("harness".into(), binding.id.clone(), json!(journal))
                .await
                .map_err(failure)?;
        }
        let stopping = matches!(
            operation,
            Operation::Character {
                event: ene_plugin::CharacterEvent::Deactivated,
                ..
            } | Operation::Harness {
                input: ene_plugin::HarnessInput::Cancel
            }
        );
        let mut failure_reason = "startup_failed";
        let outcome = async {
            if session.is_none() {
                let installed = entry.package.install(&self.cache).map_err(failure)?;
                *session = Some(
                    Worker::start(
                        installed,
                        binding.entrypoint.clone(),
                        &self.runner,
                        Target::Host,
                        self.generations.fetch_add(1, Ordering::Relaxed),
                        &cancel,
                    )
                    .await
                    .map_err(failure)?,
                );
            }
            failure_reason = "invocation_failed";
            let services = Arc::new(LocalServices {
                manager: self.clone(),
                binding: binding.clone(),
                cancel: cancel.clone(),
            });
            let (tx, mut rx) = mpsc::channel(16);
            let collect = async {
                let mut events = Vec::new();
                let mut bytes = 0;
                while let Some(event) = rx.recv().await {
                    bytes += serde_json::to_vec(&event).map_err(failure)?.len();
                    if events.len() >= MAX_EVENTS || bytes > MAX_MESSAGE_BYTES {
                        return Err(Error::invalid());
                    }
                    validate_event(&event)?;
                    events.push(serde_json::to_value(event).map_err(failure)?);
                }
                Ok::<_, Error>(events)
            };
            let validation = services.clone();
            let worker = session.as_mut().unwrap();
            let generation = worker.generation();
            let work = worker.invoke(
                uuid::Uuid::new_v4().to_string(),
                operation,
                services,
                tx,
                &cancel,
            );
            let (result, events) = tokio::join!(work, collect);
            let result = result.map_err(failure)?;
            let events = events?;
            validation
                .check()
                .await
                .map_err(|_| Error::new(ErrorKind::PermissionRequired))?;
            if self
                .ready(&binding.plugin_id, Some(&binding.revision))?
                .generation
                != entry.generation
            {
                return Err(conflict());
            }
            Ok((generation, events, result))
        }
        .await;
        let terminal = outcome.as_ref().is_ok_and(|(_, events, _)| {
            events.iter().any(|e| {
                matches!(
                    e["event"]["type"].as_str(),
                    Some("completed" | "failed" | "cancelled")
                )
            })
        });
        if (!persistent || stopping || terminal || outcome.is_err() || cancel.is_cancelled())
            && let Some(mut worker) = session.take()
        {
            worker.stop().await;
        }
        // One Host-owned path classifies and persists every unsuccessful Harness call.
        if outcome.is_err() || cancel.is_cancelled() {
            if let Some(journal) = journal {
                self.harness_finish(&binding, journal, &cancel, &lifetime, failure_reason)
                    .await?;
            }
            return Err(outcome.err().unwrap_or_else(conflict));
        }
        let (instance_generation, events, mut result) = outcome?;
        if let Some(journal) = journal {
            let task_id = self.harness_commit(&binding, journal, &events).await?;
            result = json!({"task_id":task_id,"output":result});
        }
        session.retain = persistent && !stopping && !terminal;
        Ok(PluginResult {
            generation: instance_generation,
            events: events.into_iter().map(document).collect(),
            result: document(result).into(),
            ..Default::default()
        })
    }
    pub async fn restore(&self) {
        if let Ok(records) = self.store.plugin_list("enabled".into()).await {
            // Saved identity includes the package ID; changed code never auto-enables.
            for record in records {
                if let (Some(id), Some(input)) = (
                    record.get("id").and_then(Value::as_str),
                    record.get("input"),
                ) && let Ok(input) = serde_json::from_value::<EnablePluginRequest>(input.clone())
                {
                    let _ = self.change_enabled(id.into(), input).await;
                }
            }
        }
    }
}
fn bundle(package: &Package, entrypoint: &str) -> Result<Bundle> {
    let p = package
        .entry(entrypoint)
        .map_err(failure)?
        .provider
        .as_ref()
        .ok_or_else(unsupported)?;
    Ok(Bundle {
        name: package.manifest.name.clone(),
        manifest: p.clone(),
        revision: package.revision.clone(),
    })
}
impl Plugins for Manager {
    fn list(&self) -> Vec<Plugin> {
        let mut result: Vec<Plugin> = self
            .entries
            .read()
            .map(|entries| {
                entries
                    .values()
                    .map(|e| Plugin {
                        id: e.package.manifest.id.clone(),
                        name: e.package.manifest.name.clone(),
                        version: e.package.manifest.version.clone(),
                        revision: e.package.revision.clone(),
                        state: if e.package.verify().is_ok() {
                            e.state.clone()
                        } else {
                            "changed".into()
                        },
                        error: e.error.clone(),
                        entrypoints: e
                            .package
                            .manifest
                            .entrypoints
                            .iter()
                            .map(|p| PluginEntry {
                                id: p.id.clone(),
                                target: name(p.target),
                                runtime: name(p.runtime),
                                provides: p.provides.iter().copied().map(name).collect(),
                                uses: p.uses.iter().copied().map(name).collect(),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(error) = self.discovery_error.read().ok().and_then(|e| e.clone()) {
            result.push(Plugin {
                id: "plugin-discovery".into(),
                name: "Plugin discovery".into(),
                version: "".into(),
                revision: "".into(),
                state: "failed".into(),
                error: Some(error),
                entrypoints: Vec::new(),
                ..Default::default()
            });
        }
        result
    }
    fn reload(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let result = self.scan();
            *self.discovery_error.write().map_err(failure)? = result
                .as_ref()
                .err()
                .map(|_| "invalid_manifest_or_duplicate_id".into());
            result?;
            self.stop_sessions(None).await;
            Ok(())
        })
    }
    fn enable(&self, id: String, input: EnablePluginRequest) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.change_enabled(id, input))
    }
    fn bindings(&self) -> BoxFuture<'_, Result<Vec<PluginBinding>>> {
        Box::pin(async {
            self.store
                .plugin_list("bindings".into())
                .await
                .map_err(failure)?
                .into_iter()
                .map(|v| serde_json::from_value(v).map_err(failure))
                .collect()
        })
    }
    fn bind(&self, input: PluginBinding) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let entry = self.ready(&input.plugin_id, Some(&input.revision))?;
            let endpoint = entry.package.entry(&input.entrypoint).map_err(failure)?;
            if !ene_plugin_runtime::package::identifier(&input.id)
                || endpoint.target != Target::Host
                || endpoint.provides.contains(&Extension::Character)
                    && !ene_plugin_runtime::package::identifier(&input.device_id)
                || input
                    .services
                    .iter()
                    .any(|s| !endpoint.uses.iter().any(|service| name(*service) == *s))
                || serde_json::to_vec(&input.configuration)
                    .map_err(failure)?
                    .len()
                    > 65536
                || input
                    .services
                    .iter()
                    .any(|s| !matches!(s.as_str(), "log" | "configuration" | "state"))
            {
                return Err(Error::invalid());
            }
            serde_json::from_str::<Value>(&input.configuration.json)
                .map_err(|_| Error::invalid())?;
            let snapshot = self.store.snapshot().await.map_err(failure)?;
            if !snapshot
                .companions
                .iter()
                .any(|c| c.id == input.companion_id)
            {
                return Err(Error::new(ErrorKind::NotFound));
            }
            if let Some((_, token)) = self.binding_tokens.lock().unwrap().remove(&input.id) {
                token.cancel();
            }
            let session = { self.sessions.lock().unwrap().remove(&input.id) };
            self.store
                .plugin_set(
                    "bindings".into(),
                    input.id.clone(),
                    serde_json::to_value(input).map_err(failure)?,
                )
                .await
                .map_err(failure)?;
            if let Some(session) = session
                && let Some(mut worker) = session.lock().await.take()
            {
                worker.stop().await;
            }
            Ok(())
        })
    }
    fn invoke(&self, input: PluginInvocation) -> BoxFuture<'_, Result<PluginResult>> {
        let manager = self.clone();
        Box::pin(async move {
            // Once dispatched, cancellation and journal persistence outlive the RPC waiter.
            tokio::spawn(async move { manager.invoke_owned(input).await })
                .await
                .map_err(failure)?
        })
    }
    fn restore(&self) -> BoxFuture<'_, ()> {
        Box::pin(Manager::restore(self))
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            self.stopped.cancel();
            self.stop_sessions(None).await;
            self.supervisor.shutdown().await;
        })
    }
}
struct LocalServices {
    manager: Manager,
    binding: PluginBinding,
    cancel: CancellationToken,
}
impl Services for LocalServices {
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async {
            if self.cancel.is_cancelled()
                || self
                    .manager
                    .ready(&self.binding.plugin_id, Some(&self.binding.revision))
                    .is_err()
            {
                return Err(Failure::new("revoked"));
            }
            let current = self
                .manager
                .store
                .plugin_get("bindings".into(), self.binding.id.clone())
                .await
                .map_err(|_| Failure::new("state_unavailable"))?;
            if current != serde_json::to_value(&self.binding).unwrap_or_default() {
                return Err(Failure::new("revoked"));
            }
            Ok(())
        })
    }
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async move {
            self.check().await?;
            if !self.binding.services.contains(&name(request.service())) {
                return Err(Failure::new("permission_required"));
            }
            let namespace = format!(
                "state:{}:{}:{}",
                self.binding.plugin_id, self.binding.entrypoint, self.binding.companion_id
            );
            match request {
                ServiceRequest::Log { message } if message.len() <= 4096 => Ok(Value::Null),
                ServiceRequest::Configuration => {
                    serde_json::from_str(&self.binding.configuration.json)
                        .map_err(|_| Failure::new("invalid_configuration"))
                }
                ServiceRequest::StateRead { key } if valid_key(&key) => self
                    .manager
                    .store
                    .plugin_get(namespace, key)
                    .await
                    .map_err(|_| Failure::new("state_unavailable")),
                ServiceRequest::StateWrite { key, value }
                    if valid_key(&key)
                        && serde_json::to_vec(&value).is_ok_and(|v| v.len() <= 65536) =>
                {
                    self.manager
                        .store
                        .plugin_set(namespace, key, value)
                        .await
                        .map_err(|_| Failure::new("state_unavailable"))?;
                    Ok(Value::Null)
                }
                _ => Err(Failure::new("unsupported_service")),
            }
        })
    }
}
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control)
}
fn validate_event(event: &Event) -> Result<()> {
    match event {
        Event::Character {
            action: CharacterAction::Speak { text },
            ..
        } if text.len() > 16384 => Err(Error::invalid()),
        Event::Character {
            action:
                CharacterAction::Expression { name, duration_ms }
                | CharacterAction::Motion {
                    asset: name,
                    duration_ms,
                },
            ..
        } if name.len() > 128 || *duration_ms > 60000 => Err(Error::invalid()),
        Event::Audio { chunk }
            if chunk.channels == 0
                || chunk.channels > 8
                || chunk.sample_rate < 8000
                || chunk.sample_rate > 192000 =>
        {
            Err(Error::invalid())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests;

fn document(value: Value) -> ene_rpc::JsonDocument {
    ene_rpc::JsonDocument {
        json: value.to_string(),
        ..Default::default()
    }
}
fn name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("contract name")
        .as_str()
        .expect("contract name")
        .into()
}
