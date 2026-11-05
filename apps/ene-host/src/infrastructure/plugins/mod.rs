//! Host integration: ownership and permissions belong to ene, execution to the embedded runtime.
mod acp;
mod bindings;
mod discovery;
mod harness;
mod invoke;
mod lifecycle;
mod provider;
mod sessions;
use super::{llm::plugin::manifest::Bundle, sqlite::Store};
use crate::{
    domain::{Error, ErrorKind},
    ports::{Result, inference::TaskInference},
};
use ene_plugin::Target;
use ene_plugin_runtime::{
    Package,
    instance::{Instance, Services, Supervisor},
    limits::MAX_INSTANCES,
};
use ene_rpc::{EnablePluginRequest, Plugin, PluginBinding, PluginEntry};
#[cfg(test)]
use ene_rpc::{PluginInvocation, PluginResult};
use serde_json::{Value, json};
use sessions::Sessions;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock, atomic::AtomicU64},
};
use tokio::sync::Mutex;
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
#[derive(Clone)]
pub struct Manager {
    root: PathBuf,
    store: Store,
    entries: Arc<RwLock<BTreeMap<String, Entry>>>,
    sessions: Sessions,
    // Serializes the start and the persisted commit of enable/disable, not their work.
    commit: Arc<Mutex<()>>,
    // Serializes binding replacement with the binding read and session creation.
    binding: Arc<Mutex<()>>,
    generations: Arc<AtomicU64>,
    pub(crate) supervisor: Supervisor,
    stopped: CancellationToken,
    discovery_error: Arc<RwLock<Option<String>>>,
    invocation_slots: Arc<tokio::sync::Semaphore>,
    // Attached after the application exists; the host inference service holds
    // only a weak reference back to the application.
    inference: Arc<std::sync::OnceLock<Arc<dyn TaskInference>>>,
    // External ACP agent processes keyed by binding id.
    acp: Arc<std::sync::Mutex<BTreeMap<String, Arc<sessions::AcpSlot>>>>,
    #[cfg(test)]
    pub(crate) enable_entries: Arc<std::sync::atomic::AtomicUsize>,
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
            commit: Arc::default(),
            binding: Arc::default(),
            generations: Arc::new(AtomicU64::new(1)),
            supervisor: Supervisor::default(),
            stopped: CancellationToken::new(),
            discovery_error: Arc::default(),
            invocation_slots: Arc::new(tokio::sync::Semaphore::new(MAX_INSTANCES)),
            inference: Arc::default(),
            acp: Arc::default(),
            #[cfg(test)]
            enable_entries: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        // Malformed packages are reported by reload; no code is executed during discovery.
        if manager.scan().is_err() {
            *manager.discovery_error.write().unwrap() =
                Some("invalid_manifest_or_duplicate_id".into());
        }
        manager
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
    pub(crate) fn attach_inference(&self, inference: Arc<dyn TaskInference>) {
        let _ = self.inference.set(inference);
    }
    pub(crate) fn inference(&self) -> Option<Arc<dyn TaskInference>> {
        self.inference.get().cloned()
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
impl Manager {
    pub fn list(&self) -> Vec<Plugin> {
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
    pub async fn reload(&self) -> Result<()> {
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
    }
    pub async fn harness(&self, binding_id: String) -> Result<Option<Value>> {
        let Some(loaded) = self.harness_load(&binding_id).await? else {
            return Ok(None);
        };
        serde_json::to_value(loaded.session)
            .map(Some)
            .map_err(failure)
    }
    pub fn cancel(&self) {
        self.stopped.cancel();
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.cancel();
        let result = self.stop_sessions(None).await;
        self.supervisor.shutdown().await;
        result
    }
}

/// The harness task view returned to the plugin client.
pub(super) fn harness_result(journal: &crate::domain::harness::Session) -> Value {
    let mut result = json!({
        "task_id": journal.session_id,
        "status": journal.status.name(),
    });
    if let Some(stop_reason) = journal.stop_reason {
        result["stop_reason"] = json!(stop_reason);
    }
    result
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
fn document(value: Value) -> ene_rpc::JsonDocument {
    value.into()
}
fn name(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("contract name")
        .as_str()
        .expect("contract name")
        .into()
}
