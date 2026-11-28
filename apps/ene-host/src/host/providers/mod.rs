//! The provider plugins this host has loaded.
//!
//! A plugin package is read from `<state_dir>/plugins/<id>/` at startup and
//! verified by `ene-plugin-runtime` before anything runs. The registry is then
//! a plain map from the adapter name a connection already carries to the
//! loaded provider, so the synchronous paths that resolve a connection
//! (`mind::embedding::target`, `model::connect`, `voice::tts_source`) read it
//! without awaiting.
//!
//! Loading is deliberately not lazy. A connection is resolved inside an ECS
//! frame, and a plugin's `Instance::load` is async; preloading at startup is
//! what keeps that boundary synchronous, and it bounds the instance count to
//! the plugins the operator installed rather than to the connections that
//! happen to reference them.
pub(crate) mod character;
pub(crate) mod extensions;
mod grant;
mod install;
mod running;
#[cfg(test)]
mod safety_tests;
mod services;

pub(crate) use grant::Grant;
pub(crate) use install::{install_local_package, remove_local_package};
pub(crate) use running::{ProviderRole, Running};
#[cfg(test)]
pub(crate) use services::construction_failure;
#[cfg(test)]
pub(crate) use services::http_broker;
pub(crate) use services::{PluginServices, ProviderInvocationContext};

use ene_plugin::{EntryPoint, Extension, ProviderConfiguration, Target};
use ene_plugin_runtime::Package;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::host::error::HostError;
use crate::host::settings::ConnectionKind;
use install::{is_managed_directory, is_managed_revision, package_locations};

/// Where a host looks for plugin packages unless told otherwise.
pub(crate) const PLUGINS_DIR: &str = "plugins";
/// The environment override, for a development checkout or a test.
pub(crate) const PLUGINS_DIR_ENV: &str = "ENE_HOST_PLUGINS_DIR";

/// A verified package with an entrypoint this host can select.
pub(crate) struct Loaded {
    pub(crate) package: Package,
}

fn serves(entry: &EntryPoint, extension: Extension) -> bool {
    entry.target != Target::Client
        && entry.provides.contains(&extension)
        && if extension.serves_provider() {
            entry.provider.is_some()
        } else {
            matches!(
                extension,
                Extension::Harness | Extension::Character | Extension::Hook | Extension::Tool
            )
        }
}

impl Loaded {
    /// The first entrypoint that serves this provider extension and its configuration.
    pub(crate) fn entry(&self, extension: Extension) -> Option<(&str, &ProviderConfiguration)> {
        if !extension.serves_provider() {
            return None;
        }
        let entry = self.entrypoint(extension)?;
        Some((&entry.id, entry.provider.as_ref()?))
    }

    /// The first entrypoint that serves the given non-provider extension.
    pub(crate) fn extension_entrypoint(&self, extension: Extension) -> Option<&str> {
        self.entrypoint(extension).map(|entry| entry.id.as_str())
    }

    fn entrypoint(&self, extension: Extension) -> Option<&EntryPoint> {
        self.package
            .manifest
            .entrypoints
            .iter()
            .find(|entry| serves(entry, extension))
    }
}

/// The providers a host may call, by the adapter name a connection names.
#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Providers {
    installed: Arc<RwLock<Registry>>,
}

#[derive(Default)]
struct Registry {
    registrations: HashMap<String, Registration>,
    revision: u64,
}

struct Registration {
    loaded: Option<Arc<Loaded>>,
    installation_id: Option<Arc<str>>,
    active: Option<Admission>,
}

#[derive(Default)]
struct Admission {
    lifetime: CancellationToken,
    started: HashMap<Extension, Arc<OnceCell<Arc<Running>>>>,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

impl Providers {
    /// An empty registry, for a host with no plugins installed.
    pub(crate) fn none() -> Self {
        Self::default()
    }

    /// Reads every package under `root`, keeping the ones that verify.
    ///
    /// A package that fails to load is reported and skipped: one broken plugin
    /// must not stop the host, and the connection that named it then resolves
    /// to nothing rather than to a half-loaded provider.
    pub(crate) fn load(root: &Path) -> Self {
        let directory = plugins_dir(root);
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return Self::none();
        };
        let registry = Self::none();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || is_managed_directory(&path) {
                continue;
            }
            match load_one(&path) {
                Ok((id, plugin)) if path.file_name().is_some_and(|name| name == id.as_str()) => {
                    registry.install(id, Arc::new(plugin), None, true);
                }
                Ok((id, _)) => tracing::warn!(plugin = %id, "plugin directory must match its ID"),
                Err(error) => {
                    tracing::warn!(plugin = %path.display(), "the plugin was not loaded: {error}");
                }
            }
        }
        registry
    }

    /// Loads the exact recorded revision before any plugin can start. A loose
    /// package found in the operator's plugin directory is registered once.
    pub(crate) fn restore_admissions(
        &self,
        state: &Path,
        store: &crate::host::db::Store,
    ) -> ene_store::Result<()> {
        let repo = store.plugin_repo();
        let entries = repo.plugin_entries()?;
        for entry in &entries {
            self.restore_package(state, entry);
        }
        for id in self
            .adapters()
            .filter(|id| entries.iter().all(|entry| entry.plugin_id != *id))
        {
            if let Some(loaded) = self.get(&id) {
                let package = &loaded.package;
                let entry = repo.install_plugin_entry(
                    &id,
                    &package.manifest.name,
                    &package.manifest.version,
                    &package.root.to_string_lossy(),
                    &package.revision,
                )?;
                self.install(id, loaded, Some(entry.installation_id.into()), true);
            }
        }
        Ok(())
    }

    fn restore_package(&self, state: &Path, entry: &ene_store::PluginEntry) {
        let id = &entry.plugin_id;
        if !ene_plugin_runtime::package::identifier(id) {
            return;
        }
        let [legacy, managed] = package_locations(state, id);
        let revision = &entry.revision;
        let candidate = if revision.len() == 64
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            && is_managed_revision(Path::new(&entry.path), id, revision)
        {
            load_one(&managed.join(revision))
                .ok()
                .map(|(_, loaded)| Arc::new(loaded))
        } else {
            self.get(id).filter(|loaded| loaded.package.root == legacy)
        };
        if let Some(loaded) = candidate
            && loaded.package.manifest.id == *id
            && *revision == loaded.package.revision
        {
            self.install(
                id.clone(),
                loaded,
                Some(entry.installation_id.as_str().into()),
                entry.enabled,
            );
        } else {
            let mut installed = self.installed.write();
            installed.registrations.insert(
                id.clone(),
                Registration {
                    loaded: None,
                    installation_id: None,
                    active: None,
                },
            );
            installed.revision += 1;
            tracing::warn!(plugin = %id, "the registered plugin revision is unavailable");
        }
    }

    /// The plugin a connection's `adapter` names, when one is installed.
    pub(crate) fn get(&self, adapter: &str) -> Option<Arc<Loaded>> {
        self.installed
            .read()
            .registrations
            .get(adapter)
            .and_then(|entry| entry.loaded.clone())
    }

    /// Runtime revision used by connection caches when plugin admission changes.
    pub(crate) fn revision(&self) -> u64 {
        self.installed.read().revision
    }

    /// Whether the adapter is reserved by a plugin for this extension.
    pub(crate) fn claims(&self, adapter: &str, extension: Extension) -> bool {
        self.installed
            .read()
            .registrations
            .get(adapter)
            .is_some_and(|entry| entry.claims(extension))
    }

    /// The adapters this host loaded.
    pub(crate) fn adapters(&self) -> impl Iterator<Item = String> + use<> {
        self.installed
            .read()
            .registrations
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn install(
        &self,
        id: String,
        loaded: Arc<Loaded>,
        installation_id: Option<Arc<str>>,
        enabled: bool,
    ) {
        let mut registry = self.installed.write();
        registry.registrations.insert(
            id,
            Registration {
                loaded: Some(loaded),
                installation_id,
                active: enabled.then(Admission::default),
            },
        );
        registry.revision += 1;
    }

    pub(crate) fn replace(&self, package: Package, installation_id: String) {
        let id = package.manifest.id.clone();
        match load_package(package) {
            Ok(plugin) => self.install(id, Arc::new(plugin), Some(installation_id.into()), true),
            Err(_) => self.remove(&id),
        }
    }

    pub(crate) fn set_enabled(&self, id: &str, enabled: bool) {
        let mut installed = self.installed.write();
        if let Some(entry) = installed.registrations.get_mut(id)
            && entry.loaded.is_some()
            && entry.active.is_some() != enabled
        {
            entry.active = enabled.then(Admission::default);
            installed.revision += 1;
        }
    }

    pub(crate) fn remove(&self, id: &str) {
        let mut registry = self.installed.write();
        if registry.registrations.remove(id).is_some() {
            registry.revision += 1;
        }
    }

    /// Deterministically sorted plugin IDs that provide `extension`.
    pub(crate) fn with_extension(&self, extension: Extension) -> Vec<String> {
        let mut ids: Vec<String> = self
            .installed
            .read()
            .registrations
            .iter()
            .filter(|(_, entry)| entry.active.is_some() && entry.claims(extension))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Starts `adapter`'s entrypoint for `extension`, once per adapter and
    /// extension. A second caller gets the running instance.
    pub(crate) async fn start(
        &self,
        adapter: &str,
        extension: Extension,
    ) -> Result<Arc<Running>, HostError> {
        let unavailable = || HostError::Unavailable(format!("plugin `{adapter}` is not enabled"));
        let (loaded, installation_id, cell, lifetime) = {
            let mut installed = self.installed.write();
            let entry = installed
                .registrations
                .get_mut(adapter)
                .ok_or_else(unavailable)?;
            let active = entry.active.as_mut().ok_or_else(unavailable)?;
            (
                entry.loaded.clone().ok_or_else(unavailable)?,
                entry.installation_id.clone(),
                Arc::clone(active.started.entry(extension).or_default()),
                active.lifetime.clone(),
            )
        };
        let running = cell
            .get_or_try_init(|| async {
                Running::start(loaded, installation_id, extension, lifetime.clone())
                    .await
                    .map(Arc::new)
            })
            .await?;
        if lifetime.is_cancelled() {
            return Err(unavailable());
        }
        Ok(Arc::clone(running))
    }

    /// The started instance for `adapter` and `extension`, when there is one.
    pub(crate) fn running(&self, adapter: &str, extension: Extension) -> Option<Arc<Running>> {
        self.installed
            .read()
            .registrations
            .get(adapter)?
            .started(extension)
    }

    pub(crate) fn bound_running(
        &self,
        adapter: &str,
        extension: Extension,
    ) -> Result<Option<Arc<Running>>, HostError> {
        let registry = self.installed.read();
        let Some(entry) = registry
            .registrations
            .get(adapter)
            .filter(|entry| entry.claims(extension))
        else {
            return Ok(None);
        };
        entry.started(extension).map(Some).ok_or_else(|| {
            HostError::Unavailable(format!(
                "the configured plugin `{adapter}` is unavailable for {extension:?}"
            ))
        })
    }

    /// Starts an instance for every stored connection that names a plugin.
    ///
    /// Only the plugins in use are started, so a host with an installed but
    /// unreferenced plugin pays nothing for it. A connection whose plugin
    /// fails to start is logged and skipped: the host still serves the
    /// connections that do work, and that connection resolves to nothing.
    pub(crate) async fn start_used(&self, store: &crate::host::db::Store) {
        let Ok(settings) = store.settings_repo().settings() else {
            return;
        };
        for connection in settings.connections {
            if !connection.enabled {
                continue;
            }
            let Some(extension) = connection_extension(connection.kind) else {
                continue;
            };
            let adapter = &connection.endpoint().adapter;
            if !self
                .installed
                .read()
                .registrations
                .get(adapter)
                .is_some_and(|entry| entry.active.is_some() && entry.claims(extension))
            {
                continue;
            }
            if let Err(error) = self.start(adapter, extension).await {
                tracing::warn!(
                    connection = %connection.id,
                    "the plugin provider did not start: {error}"
                );
            }
        }
    }
}

pub(crate) fn connection_extension(kind: ConnectionKind) -> Option<Extension> {
    Some(match kind {
        ConnectionKind::Embedding => Extension::Embedding,
        ConnectionKind::Llm => Extension::Llm,
        ConnectionKind::Tts => Extension::Tts,
        ConnectionKind::Stt => Extension::Stt,
        ConnectionKind::Acp => Extension::Harness,
        ConnectionKind::Mcp | ConnectionKind::Search => return None,
    })
}

impl Registration {
    fn claims(&self, extension: Extension) -> bool {
        self.loaded
            .as_ref()
            .is_none_or(|loaded| loaded.entrypoint(extension).is_some())
    }

    fn started(&self, extension: Extension) -> Option<Arc<Running>> {
        self.active
            .as_ref()?
            .started
            .get(&extension)?
            .get()
            .cloned()
    }
}

/// The directory packages live in, honouring the environment override.
pub(crate) fn plugins_dir(state: &Path) -> PathBuf {
    match std::env::var_os(PLUGINS_DIR_ENV) {
        Some(directory) if !directory.is_empty() => PathBuf::from(directory),
        _ => state.join(PLUGINS_DIR),
    }
}

fn load_one(path: &Path) -> Result<(String, Loaded), HostError> {
    let package = Package::load(path)
        .map_err(|error| HostError::Unavailable(format!("plugin package: {error}")))?;
    let id = package.manifest.id.clone();
    Ok((id, load_package(package)?))
}

fn load_package(package: Package) -> Result<Loaded, HostError> {
    if package.manifest.entrypoints.iter().any(|entry| {
        entry
            .provides
            .iter()
            .any(|&extension| serves(entry, extension))
    }) {
        return Ok(Loaded { package });
    }
    Err(HostError::Unavailable(format!(
        "the plugin `{}` serves no supported host extension",
        package.manifest.id,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        // A package is only read from a directory and a file the user owns and
        // no one else may write, so the fixture has to be that too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for directory in path.ancestors().take_while(|dir| dir.is_dir()) {
                if directory.ends_with("plugins") {
                    break;
                }
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn package(root: &Path, id: &str, provides: &str) {
        write(root.join("main.wasm"), b"\0asm");
        write(
            root.join("manifest.json"),
            serde_json::json!({
                "contract_version": ene_plugin::CONTRACT_VERSION,
                "id": id, "name": id, "version": "1",
                "entrypoints": [{
                    "id": "main", "target": "host", "file": "main.wasm",
                    "provides": [provides], "uses": ["http"],
                    "provider": {
                        "id": id, "endpoint": "http://127.0.0.1:1",
                        "auth": {"type": "none"},
                        "destinations": ["http://127.0.0.1:1"],
                        "extensions": [provides]
                    }
                }]
            })
            .to_string(),
        );
    }

    #[test]
    fn a_registry_reads_the_packages_under_its_directory() {
        let temp = tempfile::tempdir().unwrap();
        package(
            &temp.path().join(PLUGINS_DIR).join("voicevox"),
            "voicevox",
            "tts",
        );
        package(
            &temp.path().join(PLUGINS_DIR).join("local-embed"),
            "local-embed",
            "embedding",
        );
        let providers = Providers::load(temp.path());
        let mut adapters: Vec<String> = providers.adapters().collect();
        adapters.sort_unstable();
        assert_eq!(adapters, ["local-embed", "voicevox"]);
        assert_eq!(
            providers
                .get("voicevox")
                .unwrap()
                .entry(Extension::Tts)
                .map(|(entrypoint, _)| entrypoint),
            Some("main")
        );
        assert!(
            providers
                .get("voicevox")
                .unwrap()
                .entry(Extension::Llm)
                .is_none()
        );
    }

    /// One package may serve several provider extensions. Each one keeps the
    /// configuration its own entrypoint declared: an LLM endpoint must not be
    /// used for speech, and a second entrypoint must not overwrite the first.
    #[test]
    fn each_extension_keeps_its_own_provider_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(PLUGINS_DIR).join("multi");
        write(root.join("main.wasm"), b"\0asm");
        write(
            root.join("manifest.json"),
            serde_json::json!({
                "contract_version": ene_plugin::CONTRACT_VERSION,
                "id": "multi", "name": "multi", "version": "1",
                "entrypoints": [
                    {
                        "id": "llm", "target": "host", "file": "main.wasm",
                        "provides": ["llm"], "uses": ["http"],
                        "provider": {
                            "id": "multi-llm", "endpoint": "http://127.0.0.1:1",
                            "auth": {"type": "none"},
                            "destinations": ["http://127.0.0.1:1"],
                            "extensions": ["llm"]
                        }
                    },
                    {
                        "id": "speech", "target": "host", "file": "main.wasm",
                        "provides": ["tts"], "uses": ["http"],
                        "provider": {
                            "id": "multi-speech", "endpoint": "http://127.0.0.1:2",
                            "auth": {"type": "none"},
                            "destinations": ["http://127.0.0.1:2"],
                            "extensions": ["tts"]
                        }
                    }
                ]
            })
            .to_string(),
        );
        let providers = Providers::load(temp.path());
        let loaded = providers.get("multi").unwrap();
        let (entry, llm) = loaded.entry(Extension::Llm).unwrap();
        assert_eq!(
            (entry, llm.endpoint.as_str()),
            ("llm", "http://127.0.0.1:1")
        );
        let (entry, tts) = loaded.entry(Extension::Tts).unwrap();
        assert_eq!(
            (entry, tts.endpoint.as_str()),
            ("speech", "http://127.0.0.1:2")
        );
    }

    #[test]
    fn a_broken_package_is_skipped_and_the_rest_still_load() {
        let temp = tempfile::tempdir().unwrap();
        package(
            &temp.path().join(PLUGINS_DIR).join("good"),
            "good",
            "embedding",
        );
        let broken = temp.path().join(PLUGINS_DIR).join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("manifest.json"), b"not json").unwrap();
        let providers = Providers::load(temp.path());
        assert!(providers.get("good").is_some());
        assert!(providers.get("broken").is_none());
    }

    #[test]
    fn a_missing_directory_is_an_empty_registry() {
        let temp = tempfile::tempdir().unwrap();
        let providers = Providers::load(&temp.path().join("absent"));
        assert_eq!(providers.adapters().count(), 0);
    }
}
