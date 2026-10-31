//! Host integration: ownership and permissions belong to ene, execution to the embedded runtime.
mod harness;
mod provider;
use super::{
    llm::plugin::manifest::{self, Bundle},
    sqlite::Store,
};
use crate::{
    domain::{Error, ErrorKind},
    ports::{
        Result,
        plugin::{Mutations, Plugins},
    },
};
use ene_plugin::{Event, Failure, HarnessEvent, Operation, ServiceRequest};
use ene_plugin::{Extension, Target};
use ene_plugin_runtime::{
    Package,
    instance::{Instance, Services, Supervisor},
    limits::{MAX_EVENTS, MAX_INSTANCES, MAX_MESSAGE_BYTES, MAX_SESSIONS},
};
use ene_rpc::{
    EnablePluginRequest, Plugin, PluginBinding, PluginEntry, PluginInvocation, PluginResult,
};
use futures_util::{TryFutureExt, future::BoxFuture};
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
struct SessionState {
    package_id: String,
    lifetime: CancellationToken,
    worker: Mutex<Option<Instance>>,
    active: std::sync::Mutex<Option<(uuid::Uuid, CancellationToken)>>,
}
impl SessionState {
    fn new(package_id: String, lifetime: CancellationToken) -> Self {
        Self {
            package_id,
            lifetime,
            worker: Mutex::new(None),
            active: std::sync::Mutex::new(None),
        }
    }
}
type Sessions = Arc<std::sync::Mutex<BTreeMap<String, Arc<SessionState>>>>;
type Registration = (Manager, String, Arc<SessionState>);
struct Session<'a> {
    guard: tokio::sync::MutexGuard<'a, Option<Instance>>,
    retain: bool,
    registration: Registration,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl std::ops::Deref for Session<'_> {
    type Target = Option<Instance>;
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
            let (manager, key, worker) = &self.registration;
            let mut sessions = manager.sessions.lock().unwrap();
            if sessions
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, worker))
            {
                sessions.remove(key);
            }
        }
    }
}
struct ActiveInvocation {
    session: Arc<SessionState>,
    token: CancellationToken,
    id: uuid::Uuid,
}
impl ActiveInvocation {
    fn new(session: Arc<SessionState>, token: CancellationToken) -> Self {
        let id = uuid::Uuid::new_v4();
        *session.active.lock().unwrap() = Some((id, token.clone()));
        Self { session, token, id }
    }
}
impl Drop for ActiveInvocation {
    fn drop(&mut self) {
        // This guard is dropped before releasing the worker lock.
        let mut active = self.session.active.lock().unwrap();
        if active.as_ref().is_some_and(|(id, _)| *id == self.id) {
            *active = None;
        }
        self.token.cancel();
    }
}
#[derive(Clone)]
pub struct Manager {
    root: PathBuf,
    store: Store,
    entries: Arc<RwLock<BTreeMap<String, Entry>>>,
    sessions: Sessions,
    generations: Arc<AtomicU64>,
    pub(crate) supervisor: Supervisor,
    stopped: CancellationToken,
    discovery_error: Arc<RwLock<Option<String>>>,
    invocation_slots: Arc<tokio::sync::Semaphore>,
}
pub(crate) fn failure(error: impl std::fmt::Display) -> Error {
    eprintln!("ene-host plugin: {error:#}");
    Error::new(ErrorKind::ExternalFailure)
}
async fn verify(package: Package) -> Result<()> {
    tokio::task::spawn_blocking(move || package.verify())
        .await
        .map_err(failure)?
        .map_err(|error| {
            if matches!(error, ene_plugin_runtime::Error::Changed) {
                conflict()
            } else {
                failure(error)
            }
        })
}
fn invocation_failure(error: ene_plugin_runtime::Error) -> Error {
    if matches!(&error, ene_plugin_runtime::Error::Service(failure) if failure.code == "state_unavailable")
    {
        Error::new(ErrorKind::StorageFailure)
    } else {
        failure(error)
    }
}
fn storage_failure(error: impl std::fmt::Display) -> Error {
    eprintln!("ene-host plugin storage: {error:#}");
    Error::new(ErrorKind::StorageFailure)
}
fn conflict() -> Error {
    Error::new(ErrorKind::StateConflict)
}
fn unsupported() -> Error {
    Error::new(ErrorKind::UnsupportedCapability)
}
impl Manager {
    pub fn discover(store: Store) -> Self {
        let data = super::paths::data();
        let root = std::env::var_os("ENE_PLUGIN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("plugins"));
        let manager = Self {
            root,
            store,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            sessions: Arc::default(),
            generations: Arc::new(AtomicU64::new(1)),
            supervisor: Supervisor::default(),
            stopped: CancellationToken::new(),
            discovery_error: Arc::default(),
            invocation_slots: Arc::new(tokio::sync::Semaphore::new(MAX_INSTANCES)),
        };
        // Malformed packages are reported by reload; no code is executed during discovery.
        if manager.scan().is_err() {
            *manager.discovery_error.write().unwrap() =
                Some("invalid_manifest_or_duplicate_id".into());
        }
        manager
    }
    fn scan(&self) -> Result<()> {
        let (packages, errors) = self.load_packages()?;
        self.apply_scan(packages, errors)
    }
    fn load_packages(&self) -> Result<(BTreeMap<String, Package>, Vec<String>)> {
        let mut packages = BTreeMap::new();
        let mut provider_ids = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        if self.root.exists() {
            let mut paths = std::fs::read_dir(&self.root)
                .map_err(failure)?
                .map(|entry| entry.map(|entry| entry.path()).map_err(failure))
                .collect::<Result<Vec<_>>>()?;
            paths.sort();
            for path in paths {
                if !std::fs::symlink_metadata(&path).map_err(failure)?.is_dir() {
                    continue;
                }
                let package = match Package::load(&path) {
                    Ok(package) => package,
                    Err(error) => {
                        errors.push(format!(
                            "{}: {error}",
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("package")
                        ));
                        continue;
                    }
                };
                let mut package_providers = Vec::new();
                let mut provider_error = false;
                for entry in &package.manifest.entrypoints {
                    if let Some(provider) = &entry.provider {
                        if manifest::validate(provider).is_err()
                            || provider_ids.contains(&provider.id)
                        {
                            provider_error = true;
                            break;
                        }
                        package_providers.push(provider.id.clone());
                    }
                }
                if provider_error {
                    errors.push(format!("{}: provider", package.manifest.id));
                    continue;
                }
                let id = package.manifest.id.clone();
                if packages.contains_key(&id) {
                    errors.push(format!("{id}: duplicate package id"));
                    continue;
                }
                for provider in package_providers {
                    provider_ids.insert(provider);
                }
                packages.insert(id, package);
            }
        }
        Ok((packages, errors))
    }
    /// Merge a finished scan into the entries as they are now, not as they were when it started.
    fn apply_scan(&self, packages: BTreeMap<String, Package>, errors: Vec<String>) -> Result<()> {
        let mut entries = self.entries.write().map_err(failure)?;
        let mut next = BTreeMap::new();
        for (id, package) in packages {
            let entry = match entries.get(&id) {
                // Keep the management state; only the verified file snapshot is refreshed.
                Some(e)
                    if e.package.root == package.root && e.package.revision == package.revision =>
                {
                    Entry {
                        package,
                        ..e.clone()
                    }
                }
                _ => Entry {
                    package,
                    state: "disabled".into(),
                    error: None,
                    cancellation: self.stopped.child_token(),
                    generation: self.generations.fetch_add(1, Ordering::Relaxed),
                    capabilities: BTreeMap::new(),
                },
            };
            next.insert(id, entry);
        }
        for (id, entry) in entries.iter() {
            if !next
                .get(id)
                .is_some_and(|e| e.generation == entry.generation)
            {
                entry.cancellation.cancel();
            }
        }
        *entries = next;
        drop(entries);
        *self.discovery_error.write().map_err(failure)? = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
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
        let entries: Vec<Entry> = self
            .entries
            .read()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();
        entries
            .iter()
            .flat_map(|e| {
                let current = e.package.current();
                e.package
                    .manifest
                    .entrypoints
                    .iter()
                    .filter_map(move |entry| {
                        if entry.target != Target::Host {
                            return None;
                        }
                        let b = bundle(&e.package, &entry.id).ok()?;
                        let state = if current { &e.state } else { "changed" };
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
    }
    async fn stop_session(&self, key: &str, session: &Arc<SessionState>) -> Result<()> {
        session.lifetime.cancel();
        let mut worker = session.worker.lock().await;
        worker.take();
        if self
            .sessions
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            // Keep the stopped registration until its terminal journal is durable.
            self.harness_stop(key, "plugin_stopped").await?;
            self.sessions.lock().unwrap().remove(key);
        }
        Ok(())
    }
    async fn stop_sessions(&self, package: Option<&str>) -> Result<()> {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| package.is_none_or(|p| p == session.package_id))
            .map(|(key, session)| {
                session.lifetime.cancel();
                (key.clone(), session.clone())
            })
            .collect();
        let mut result = Ok(());
        for (key, session) in sessions {
            let stopped = self.stop_session(&key, &session).await;
            if result.is_ok() {
                result = stopped;
            }
        }
        result
    }
    async fn change_enabled(&self, id: String, input: EnablePluginRequest) -> Result<()> {
        if input.enabled {
            let package = self
                .entries
                .read()
                .map_err(failure)?
                .get(&id)
                .ok_or_else(unsupported)?
                .package
                .clone();
            verify(package).await?;
        }
        let (package, generation, cancel) = {
            let mut entries = self.entries.write().map_err(failure)?;
            let e = entries.get_mut(&id).ok_or_else(unsupported)?;
            if e.state == "unavailable" {
                return Err(unsupported());
            }
            if e.package.revision != input.revision {
                return Err(conflict());
            }
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
        self.stop_sessions(Some(&id)).await?;
        let result = async {
            let mut capabilities = BTreeMap::new();
            if input.enabled {
                for entry in package
                    .manifest
                    .entrypoints
                    .iter()
                    .filter(|e| e.target == Target::Host)
                {
                    let verified = package.clone();
                    let mut worker = Instance::load(
                        verified,
                        entry.id.clone(),
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
                    e.error = Some("Wasmプラグインの読み込みまたは初期化に失敗しました".into());
                    e.cancellation.cancel();
                }
            }
        }
        result?;
        self.store
            .plugin_set("enabled".into(), id.clone(), json!({"id":id,"input":input}))
            .await
            .map_err(storage_failure)?;
        Ok(())
    }
    async fn invoke_inner(
        &self,
        input: PluginInvocation,
        mutations: Arc<dyn Mutations>,
    ) -> Result<PluginResult> {
        let binding: PluginBinding = serde_json::from_value(
            self.store
                .plugin_get("bindings".into(), input.binding_id)
                .await
                .map_err(storage_failure)?,
        )
        .map_err(failure)?;
        let entry = self.ready(&binding.plugin_id, Some(&binding.revision))?;
        let operation: Operation =
            serde_json::from_str(&input.operation.json).map_err(|_| Error::invalid())?;
        let endpoint = entry.package.entry(&binding.entrypoint).map_err(failure)?;
        if !endpoint.provides.contains(&operation.extension())
            || matches!(
                operation,
                Operation::Generate { .. } | Operation::ListModels { .. }
            )
        {
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
                // Window observation belongs to the desktop adapter, not host bindings.
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
        // Cancel reaches the active invocation before waiting for its worker lock.
        let cancelling = matches!(
            &operation,
            Operation::Harness {
                input: ene_plugin::HarnessInput::Cancel
            }
        );
        if cancelling
            && let Some(session) = self.sessions.lock().unwrap().get(&binding.id)
            && let Some((_, token)) = session.active.lock().unwrap().as_ref()
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
        let key = binding.id.clone();
        let worker = {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= MAX_SESSIONS && !sessions.contains_key(&key) {
                return Err(Error::unavailable());
            }
            sessions
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(SessionState::new(
                        binding.plugin_id.clone(),
                        entry.cancellation.child_token(),
                    ))
                })
                .clone()
        };
        let lifetime = &worker.lifetime;
        let guard = worker.worker.lock().await;
        let mut session = Session {
            retain: guard.is_some() || lifetime.is_cancelled(),
            guard,
            registration: (self.clone(), key.clone(), worker.clone()),
            _permit: permit,
        };
        if lifetime.is_cancelled() && !cancelling {
            return Err(Error::unavailable());
        }
        if !cancelling
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
            let events = vec![Event::Harness {
                event: HarnessEvent::Cancelled,
            }];
            let task_id = self
                .harness_cancel(&binding, journal, &events)
                .await
                .inspect_err(|error| {
                    if error.kind == ErrorKind::StorageFailure {
                        session.take();
                        lifetime.cancel();
                        session.retain = true;
                    }
                })?;
            session.retain = false;
            if let Some(mut worker) = session.take() {
                worker.stop().await;
            }
            return Ok(PluginResult {
                generation: entry.generation,
                events: events
                    .into_iter()
                    .map(|event| serde_json::to_value(event).map(document).map_err(failure))
                    .collect::<Result<_>>()?,
                result: document(json!({"task_id": task_id})).into(),
                ..Default::default()
            });
        }
        let cancel = lifetime.child_token();
        let _invocation = ActiveInvocation::new(worker.clone(), cancel.clone());
        let journal = if let Operation::Harness { input } = &operation {
            Some(self.harness_begin(&binding, input).await?)
        } else {
            None
        };
        if let Some(journal) = &journal {
            self.store
                .plugin_set("harness".into(), binding.id.clone(), json!(journal))
                .await
                .map_err(storage_failure)?;
        }
        session.retain = false;
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
            if session.is_some() && matches!(&operation, Operation::Speech { .. }) {
                session
                    .as_mut()
                    .unwrap()
                    .initialize(&cancel)
                    .await
                    .map_err(failure)?;
            }
            if session.is_none() {
                let verified = entry.package.clone();
                *session = Some(
                    Instance::load(
                        verified,
                        binding.entrypoint.clone(),
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
                mutations,
            });
            let (tx, mut rx) = mpsc::channel(16);
            let collect = async move {
                let mut events = Vec::new();
                let mut bytes = 0;
                while let Some(event) = rx.recv().await {
                    bytes += serde_json::to_vec(&event).map_err(failure)?.len();
                    if events.len() >= MAX_EVENTS || bytes > MAX_MESSAGE_BYTES {
                        return Err(Error::invalid());
                    }
                    events.push(event);
                }
                Ok::<_, Error>(events)
            };
            let validation = services.clone();
            let worker = session.as_mut().unwrap();
            let generation = worker.generation();
            let work = worker
                .invoke(operation, services, tx, &cancel)
                .map_err(invocation_failure);
            let (events, mut result) = tokio::try_join!(collect, work)?;
            validation.check().await.map_err(|failure| {
                if failure.code == "state_unavailable" {
                    Error::new(ErrorKind::StorageFailure)
                } else {
                    Error::new(ErrorKind::PermissionRequired)
                }
            })?;
            if self
                .ready(&binding.plugin_id, Some(&binding.revision))?
                .generation
                != entry.generation
            {
                return Err(conflict());
            }
            if let Some(journal) = &journal {
                failure_reason = "commit_failed";
                let task_id = self
                    .harness_commit(&binding.id, journal.clone(), &events)
                    .await?;
                result = json!({"task_id":task_id,"output":result});
            }
            Ok((generation, events, result))
        }
        .await;
        let terminal = outcome.as_ref().is_ok_and(|(_, events, _)| {
            events.iter().any(|e| {
                matches!(
                    e,
                    Event::Harness {
                        event: HarnessEvent::Completed { .. }
                            | HarnessEvent::Failed { .. }
                            | HarnessEvent::Cancelled
                    }
                )
            })
        });
        if (stopping || terminal || outcome.is_err() || cancel.is_cancelled())
            && let Some(mut worker) = session.take()
        {
            worker.stop().await;
        }
        // One Host-owned path classifies and persists every unsuccessful Harness call.
        if outcome.is_err() || cancel.is_cancelled() {
            if let Some(journal) = journal {
                self.harness_finish(&binding, journal, &cancel, lifetime, failure_reason)
                    .await
                    .inspect_err(|_| {
                        lifetime.cancel();
                        session.retain = true;
                    })?;
            }
            return Err(outcome.err().unwrap_or_else(conflict));
        }
        let (instance_generation, events, result) = outcome?;
        session.retain = !stopping && !terminal;
        Ok(PluginResult {
            generation: instance_generation,
            events: events
                .into_iter()
                .map(|event| serde_json::to_value(event).map(document).map_err(failure))
                .collect::<Result<_>>()?,
            result: document(result).into(),
            ..Default::default()
        })
    }
    pub async fn restore(&self) -> Result<()> {
        // Guest memory cannot survive a host restart; finalize its saved tasks first.
        for binding in self.bindings().await? {
            self.harness_stop(&binding.id, "host_interrupted").await?;
        }
        let records = self
            .store
            .plugin_list("enabled".into())
            .await
            .map_err(storage_failure)?;
        // Saved identity includes the package ID; changed code never auto-enables.
        for record in records {
            if let (Some(id), Some(input)) = (
                record.get("id").and_then(Value::as_str),
                record.get("input"),
            ) && let Ok(input) = serde_json::from_value::<EnablePluginRequest>(input.clone())
                && let Err(error) = self.change_enabled(id.into(), input).await
                && error.kind == ErrorKind::StorageFailure
            {
                return Err(error);
            }
        }
        Ok(())
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
        let entries: Vec<Entry> = self
            .entries
            .read()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();
        let mut result: Vec<Plugin> = entries
            .iter()
            .map(|e| Plugin {
                id: e.package.manifest.id.clone(),
                name: e.package.manifest.name.clone(),
                version: e.package.manifest.version.clone(),
                revision: e.package.revision.clone(),
                state: if e.package.current() {
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
                        provides: p.provides.iter().copied().map(name).collect(),
                        uses: p.uses.iter().copied().map(name).collect(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
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
            if self.stopped.is_cancelled() {
                return Err(Error::unavailable());
            }
            let manager = self.clone();
            let scan = tokio::task::spawn_blocking(move || manager.scan())
                .await
                .map_err(failure)?;
            if let Err(error) = scan {
                *self.discovery_error.write().map_err(failure)? =
                    Some("invalid_manifest_or_duplicate_id".into());
                return Err(error);
            }
            self.stop_sessions(None).await?;
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
                .map_err(storage_failure)?
                .into_iter()
                .map(|v| serde_json::from_value(v).map_err(failure))
                .collect()
        })
    }
    fn bind(&self, input: PluginBinding) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let entry = self.ready(&input.plugin_id, Some(&input.revision))?;
            verify(entry.package.clone()).await?;
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
            let snapshot = self.store.snapshot().await.map_err(storage_failure)?;
            if !snapshot
                .companions
                .iter()
                .any(|c| c.id == input.companion_id)
            {
                return Err(Error::new(ErrorKind::NotFound));
            }
            let session = self.sessions.lock().unwrap().get(&input.id).cloned();
            if let Some(session) = session {
                self.stop_session(&input.id, &session).await?;
            } else {
                self.harness_stop(&input.id, "plugin_stopped").await?;
            }
            self.store
                .plugin_set(
                    "bindings".into(),
                    input.id.clone(),
                    serde_json::to_value(input).map_err(failure)?,
                )
                .await
                .map_err(storage_failure)?;
            Ok(())
        })
    }
    fn invoke(
        &self,
        input: PluginInvocation,
        mutations: Arc<dyn Mutations>,
    ) -> BoxFuture<'_, Result<PluginResult>> {
        Box::pin(self.invoke_inner(input, mutations))
    }
    fn restore(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(Manager::restore(self))
    }
    fn cancel(&self) {
        self.stopped.cancel();
    }
    fn shutdown(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            self.cancel();
            let result = self.stop_sessions(None).await;
            self.supervisor.shutdown().await;
            result
        })
    }
}
struct LocalServices {
    manager: Manager,
    binding: PluginBinding,
    cancel: CancellationToken,
    mutations: Arc<dyn Mutations>,
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
                ServiceRequest::Log { message } if message.len() <= 4096 => {
                    eprintln!("ene-host plugin [{}]: {message:?}", self.binding.plugin_id);
                    Ok(Value::Null)
                }
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
                    let store = self.manager.store.clone();
                    self.mutations
                        .submit(Box::pin(async move {
                            store
                                .plugin_set(namespace, key, value)
                                .await
                                .map_err(super::sqlite::core_error)
                        }))
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
