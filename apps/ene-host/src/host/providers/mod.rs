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
mod grant;
#[cfg(test)]
mod safety_tests;
mod services;

pub(crate) use grant::Grant;
#[cfg(test)]
pub(crate) use services::construction_failure;
pub(crate) use services::{BrokerServices, http_broker};

use ene_plugin::{Extension, ProviderConfiguration};
use ene_plugin_runtime::{Package, instance::Instance};
/// Started instances, keyed by the adapter and extension they serve.
type RunningMap = HashMap<(String, Extension), Arc<Running>>;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

use crate::host::error::HostError;

/// Where a host looks for plugin packages unless told otherwise.
pub(crate) const PLUGINS_DIR: &str = "plugins";
/// The environment override, for a development checkout or a test.
pub(crate) const PLUGINS_DIR_ENV: &str = "ENE_HOST_PLUGINS_DIR";

/// One loaded provider package.
pub(crate) struct Loaded {
    pub(crate) package: Package,
    /// Every entrypoint that serves a provider extension, by extension.
    entries: HashMap<Extension, String>,
    pub(crate) provider: ProviderConfiguration,
}

impl Loaded {
    /// The entrypoint that serves `extension`, when this package serves it.
    pub(crate) fn entry(&self, extension: Extension) -> Option<&str> {
        self.entries.get(&extension).map(String::as_str)
    }
}

/// The providers a host may call, by the adapter name a connection names.
#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Providers {
    loaded: Arc<HashMap<String, Arc<Loaded>>>,
    /// Started instances, by adapter and extension. A connection resolves to
    /// one of these synchronously; the start happens at boot.
    running: Arc<Mutex<RunningMap>>,
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
        let mut loaded = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            match load_one(&path) {
                Ok((id, plugin)) => {
                    loaded.insert(id, Arc::new(plugin));
                }
                Err(error) => {
                    tracing::warn!(plugin = %path.display(), "the plugin was not loaded: {error}");
                }
            }
        }
        Self {
            loaded: Arc::new(loaded),
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The plugin a connection's `adapter` names, when one is installed.
    pub(crate) fn get(&self, adapter: &str) -> Option<&Arc<Loaded>> {
        self.loaded.get(adapter)
    }

    /// The adapters this host loaded.
    pub(crate) fn adapters(&self) -> impl Iterator<Item = &str> {
        self.loaded.keys().map(String::as_str)
    }

    /// Starts `adapter`'s entrypoint for `extension`, once per adapter and
    /// extension. A second caller gets the running instance.
    pub(crate) async fn start(
        &self,
        adapter: &str,
        extension: Extension,
    ) -> Result<Arc<Running>, HostError> {
        let key = (adapter.to_owned(), extension);
        if let Some(running) = self
            .running
            .lock()
            .map_err(|_| HostError::unavailable("the provider registry is poisoned"))?
            .get(&key)
        {
            return Ok(Arc::clone(running));
        }
        let loaded = self.get(adapter).ok_or_else(|| {
            HostError::unavailable(format!("no plugin is installed as `{adapter}`"))
        })?;
        let running = Arc::new(Running::start(Arc::clone(loaded), extension, 1).await?);
        self.running
            .lock()
            .map_err(|_| HostError::unavailable("the provider registry is poisoned"))?
            .insert(key, Arc::clone(&running));
        Ok(running)
    }

    /// The started instance for `adapter` and `extension`, when there is one.
    pub(crate) fn running(&self, adapter: &str, extension: Extension) -> Option<Arc<Running>> {
        self.running
            .lock()
            .ok()?
            .get(&(adapter.to_owned(), extension))
            .map(Arc::clone)
    }

    /// Starts an instance for every stored connection that names a plugin.
    ///
    /// Only the plugins in use are started, so a host with an installed but
    /// unreferenced plugin pays nothing for it. A connection whose plugin
    /// fails to start is logged and skipped: the host still serves the
    /// connections that do work, and that connection resolves to nothing.
    pub(crate) async fn start_used(&self, store: &crate::host::db::Store) {
        let Ok(settings) = store.settings() else {
            return;
        };
        for connection in settings.connections {
            let adapter = connection.endpoint().adapter.clone();
            if self.get(&adapter).is_none() {
                continue;
            }
            let extension = match connection.kind {
                crate::host::settings::ConnectionKind::Embedding => Extension::Embedding,
                crate::host::settings::ConnectionKind::Llm => Extension::Llm,
                crate::host::settings::ConnectionKind::Tts => Extension::Tts,
                crate::host::settings::ConnectionKind::Stt => Extension::Stt,
                // MCP and ACP are transports, not provider plugins.
                _ => continue,
            };
            if !connection.enabled {
                continue;
            }
            if let Err(error) = self.start(&adapter, extension).await {
                tracing::warn!(
                    connection = %connection.id,
                    "the plugin provider did not start: {error}"
                );
            }
        }
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
        .map_err(|error| HostError::unavailable(format!("plugin package: {error}")))?;
    let id = package.manifest.id.clone();
    let mut entries = HashMap::new();
    let mut provider = None;
    for entrypoint in &package.manifest.entrypoints {
        let Some(configuration) = &entrypoint.provider else {
            continue;
        };
        for extension in &entrypoint.provides {
            if extension.serves_provider()
                && entries.insert(*extension, entrypoint.id.clone()).is_none()
            {
                provider = Some(configuration.clone());
            }
        }
    }
    let provider = provider.ok_or_else(|| {
        HostError::unavailable(format!("the plugin `{id}` serves no provider extension"))
    })?;
    Ok((
        id,
        Loaded {
            package,
            entries,
            provider,
        },
    ))
}

/// One running provider: its loaded package and the instance that answers.
///
/// An instance is not `Sync`, so calls serialize behind a mutex. One provider
/// serves one connection, and a call is short, so this is not a bottleneck.
pub(crate) struct Running {
    pub(crate) loaded: Arc<Loaded>,
    instance: tokio::sync::Mutex<Instance>,
    cancel: CancellationToken,
}

impl Running {
    /// Starts the entrypoint that serves `extension`.
    pub(crate) async fn start(
        loaded: Arc<Loaded>,
        extension: Extension,
        generation: u64,
    ) -> Result<Self, HostError> {
        let entrypoint = loaded.entry(extension).ok_or_else(|| {
            HostError::unavailable(format!(
                "the plugin `{}` serves no {extension:?}",
                loaded.package.manifest.id
            ))
        })?;
        let cancel = CancellationToken::new();
        let mut instance = Instance::load(
            loaded.package.clone(),
            entrypoint.to_owned(),
            generation,
            &cancel,
        )
        .await
        .map_err(|error| HostError::unavailable(format!("plugin load: {error}")))?;
        instance
            .initialize(&cancel)
            .await
            .map_err(|error| HostError::unavailable(format!("plugin initialize: {error}")))?;
        Ok(Self {
            loaded,
            instance: tokio::sync::Mutex::new(instance),
            cancel,
        })
    }

    /// Runs one operation on the plugin and returns its reply document.
    pub(crate) async fn invoke(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn ene_plugin_runtime::instance::Services>,
    ) -> Result<serde_json::Value, HostError> {
        let (sender, mut events) = tokio::sync::mpsc::channel(16);
        // A caller that wants no stream still must drain it, or a plugin that
        // emits would block on a full channel.
        tokio::spawn(async move { while events.recv().await.is_some() {} });
        self.call(operation, services, sender).await
    }

    /// Runs one operation and hands its events to `events` as they arrive, so
    /// a streamed answer reaches the caller while the plugin is still
    /// producing it.
    pub(crate) async fn invoke_streaming(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn ene_plugin_runtime::instance::Services>,
        events: tokio::sync::mpsc::Sender<ene_plugin::Event>,
    ) -> Result<serde_json::Value, HostError> {
        self.call(operation, services, events).await
    }

    async fn call(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn ene_plugin_runtime::instance::Services>,
        events: tokio::sync::mpsc::Sender<ene_plugin::Event>,
    ) -> Result<serde_json::Value, HostError> {
        let mut instance = self.instance.lock().await;
        instance
            .invoke(operation, services, events, &self.cancel)
            .await
            .map_err(|error| HostError::unavailable(format!("plugin call: {error}")))
    }

    /// The provider configuration this instance answers under.
    pub(crate) fn provider(&self) -> &ProviderConfiguration {
        &self.loaded.provider
    }
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
        let mut adapters: Vec<&str> = providers.adapters().collect();
        adapters.sort_unstable();
        assert_eq!(adapters, ["local-embed", "voicevox"]);
        assert_eq!(
            providers.get("voicevox").unwrap().entry(Extension::Tts),
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
