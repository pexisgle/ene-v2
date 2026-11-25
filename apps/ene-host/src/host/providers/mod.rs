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
pub(crate) mod extensions;
mod grant;
mod install;
#[cfg(test)]
mod safety_tests;
mod services;

pub(crate) use grant::Grant;
pub(crate) use install::{install_local_package, remove_local_package};
#[cfg(test)]
pub(crate) use services::construction_failure;
#[cfg(test)]
pub(crate) use services::{BrokerServices, http_broker};
pub(crate) use services::{PluginServices, ProviderInvocationContext};

use ene_plugin::{Extension, ProviderConfiguration};
use ene_plugin_runtime::{Package, instance::Instance};
use parking_lot::Mutex;
use rig_core::providers::registry::ProviderId;
/// Startup cells, keyed by the adapter and extension they serve.
type RunningMap = HashMap<(String, Extension), Arc<tokio::sync::OnceCell<Arc<Running>>>>;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    /// Every entrypoint that serves a provider extension, with the provider
    /// configuration that entrypoint declared. One package may serve several
    /// extensions with different endpoints and credentials.
    entries: HashMap<Extension, (String, ProviderConfiguration)>,
    /// The entrypoint that serves the harness extension. A harness is not a
    /// provider: it carries no endpoint or credential, so it lives apart from
    /// `entries` and `Running` holds no provider configuration for it.
    harness: Option<String>,
    /// Non-provider extension entrypoints (`Character`, `Hook`, `Tool`).
    extensions: HashMap<Extension, String>,
}

impl Loaded {
    /// The entrypoint that serves `extension` and its provider configuration.
    pub(crate) fn entry(&self, extension: Extension) -> Option<(&str, &ProviderConfiguration)> {
        self.entries
            .get(&extension)
            .map(|(entrypoint, provider)| (entrypoint.as_str(), provider))
    }

    /// The entrypoint that serves the harness extension, when one is declared.
    pub(crate) fn harness_entrypoint(&self) -> Option<&str> {
        self.harness.as_deref()
    }

    /// The entrypoint that serves a non-provider extension (`Character`, `Hook`, `Tool`).
    pub(crate) fn extension_entrypoint(&self, extension: Extension) -> Option<&str> {
        if extension == Extension::Harness {
            return self.harness_entrypoint();
        }
        self.extensions.get(&extension).map(String::as_str)
    }
}

/// The providers a host may call, by the adapter name a connection names.
#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct Providers {
    loaded: Arc<HashMap<String, Arc<Loaded>>>,
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

    /// Deterministically sorted plugin IDs that provide `extension`.
    pub(crate) fn with_extension(&self, extension: Extension) -> Vec<String> {
        let mut ids: Vec<String> = self
            .loaded
            .iter()
            .filter(|(_, loaded)| {
                loaded.entry(extension).is_some()
                    || loaded.extension_entrypoint(extension).is_some()
            })
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
        let loaded = self.get(adapter).ok_or_else(|| {
            HostError::Unavailable(format!("no plugin is installed as `{adapter}`"))
        })?;
        let cell = {
            let mut running = self.running.lock();
            Arc::clone(
                running
                    .entry((adapter.to_owned(), extension))
                    .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
            )
        };
        let running = cell
            .get_or_try_init(|| async {
                Running::start(Arc::clone(loaded), extension)
                    .await
                    .map(Arc::new)
            })
            .await?;
        Ok(Arc::clone(running))
    }

    /// The started instance for `adapter` and `extension`, when there is one.
    pub(crate) fn running(&self, adapter: &str, extension: Extension) -> Option<Arc<Running>> {
        self.running
            .lock()
            .get(&(adapter.to_owned(), extension))?
            .get()
            .map(Arc::clone)
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
            let adapter = connection.endpoint().adapter.clone();
            if self.get(&adapter).is_none() {
                continue;
            }
            let extension = match connection.kind {
                crate::host::settings::ConnectionKind::Embedding => Extension::Embedding,
                crate::host::settings::ConnectionKind::Llm => Extension::Llm,
                crate::host::settings::ConnectionKind::Tts => Extension::Tts,
                crate::host::settings::ConnectionKind::Stt => Extension::Stt,
                // An ACP connection names a harness plugin through its
                // endpoint adapter; the hand that drives it starts here with
                // every other used plugin. MCP is a transport, not a plugin.
                crate::host::settings::ConnectionKind::Acp => Extension::Harness,
                crate::host::settings::ConnectionKind::Mcp
                | crate::host::settings::ConnectionKind::Search => continue,
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
        .map_err(|error| HostError::Unavailable(format!("plugin package: {error}")))?;
    let id = package.manifest.id.clone();
    let mut entries = HashMap::new();
    let mut harness = None;
    let mut extensions = HashMap::new();
    for entrypoint in &package.manifest.entrypoints {
        if entrypoint.target == ene_plugin::Target::Client {
            continue;
        }
        if entrypoint.provides.contains(&Extension::Harness) {
            harness.get_or_insert_with(|| entrypoint.id.clone());
        }
        for ext in [Extension::Character, Extension::Hook, Extension::Tool] {
            if entrypoint.provides.contains(&ext) {
                extensions
                    .entry(ext)
                    .or_insert_with(|| entrypoint.id.clone());
            }
        }
        let Some(configuration) = &entrypoint.provider else {
            continue;
        };
        for extension in &entrypoint.provides {
            if extension.serves_provider() {
                entries
                    .entry(*extension)
                    .or_insert_with(|| (entrypoint.id.clone(), configuration.clone()));
            }
        }
    }
    if entries.is_empty() && harness.is_none() && extensions.is_empty() {
        return Err(HostError::Unavailable(format!(
            "the plugin `{id}` serves no supported host extension"
        )));
    }
    Ok((
        id,
        Loaded {
            package,
            entries,
            harness,
            extensions,
        },
    ))
}

/// What a running provider-plugin instance is answering as.
pub(crate) enum RunningRole {
    /// The extension serves one provider; both values come from its manifest.
    /// Boxed because the configuration dwarfs the unit variant.
    Provider(Box<ProviderRole>),
    /// A tool-only extension: it has no provider configuration at all.
    Harness,
}

/// The provider metadata a provider extension declares once, in its manifest.
pub(crate) struct ProviderRole {
    pub(crate) configuration: ProviderConfiguration,
    /// The catalog vendor this provider's models are filed under. A plugin
    /// serves a provider rig does not ship, so it is declared once here and
    /// every model spec the host builds for it names it.
    pub(crate) id: ProviderId,
}

/// One running provider: its loaded package and the instance that answers.
///
/// An instance is not `Sync`, so calls serialize behind a mutex. One provider
/// serves one connection, and a call is short, so this is not a bottleneck.
///
pub(crate) struct Running {
    /// Present only when the extension serves a provider. One field rather
    /// than two `Option`s, so "has a configuration but no vendor name" cannot
    /// be constructed.
    role: RunningRole,
    instance: tokio::sync::Mutex<Option<Instance>>,
    package: Package,
    entrypoint: String,
    cancel: CancellationToken,
    /// A caller dropped a call before it reported. The next request releases
    /// the dead instance and reloads; a call that failed on its own keeps the
    /// old policy and is not restarted automatically.
    cancelled: AtomicBool,
}

impl Running {
    /// Starts the entrypoint that serves `extension`.
    pub(crate) async fn start(
        loaded: Arc<Loaded>,
        extension: Extension,
    ) -> Result<Self, HostError> {
        let (entrypoint, role) = if !extension.serves_provider() {
            let entrypoint = loaded.extension_entrypoint(extension).ok_or_else(|| {
                HostError::Unavailable(format!(
                    "the plugin `{}` serves no {extension:?} entrypoint",
                    loaded.package.manifest.id
                ))
            })?;
            (entrypoint.to_owned(), RunningRole::Harness)
        } else {
            let (entrypoint, provider) = loaded.entry(extension).ok_or_else(|| {
                HostError::Unavailable(format!(
                    "the plugin `{}` serves no {extension:?}",
                    loaded.package.manifest.id
                ))
            })?;
            let provider_id = ProviderId::declare(&provider.id).map_err(|error| {
                HostError::Unavailable(format!(
                    "the plugin provider id `{}` is not a vendor name: {error}",
                    provider.id
                ))
            })?;
            (
                entrypoint.to_owned(),
                RunningRole::Provider(Box::new(ProviderRole {
                    configuration: provider.clone(),
                    id: provider_id,
                })),
            )
        };
        let package = loaded.package.clone();
        let cancel = CancellationToken::new();
        // `Instance::load` runs the guest's initialization hook, so this is the
        // one place `ene_initialize` is called for an instance.
        let instance = Instance::load(package.clone(), entrypoint.clone(), &cancel)
            .await
            .map_err(|error| HostError::Unavailable(format!("plugin load: {error}")))?;
        Ok(Self {
            role,
            instance: tokio::sync::Mutex::new(Some(instance)),
            package,
            entrypoint,
            cancel,
            cancelled: AtomicBool::new(false),
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
        let mut slot = self.instance.lock().await;
        if self.cancelled.swap(false, Ordering::SeqCst)
            && slot.as_ref().is_none_or(|instance| !instance.is_usable())
        {
            // A dropped caller cancelled the previous call before it could
            // report, leaving the instance without its plugin. Drop it first,
            // releasing its execution slot, then load a fresh instance for the
            // next, distinct request.
            *slot = None;
            *slot = Some(self.reload().await?);
        }
        let Some(instance) = slot.as_mut().filter(|instance| instance.is_usable()) else {
            return Err(HostError::Unavailable(
                ("the plugin is unavailable after a failed call; it is not restarted automatically")
                    .into(),
            ));
        };
        instance
            .invoke(operation, services, events, &self.cancel)
            .await
            .map_err(|error| HostError::Unavailable(format!("plugin call: {error}")))
    }

    /// Notes that a caller dropped a call before it reported. The next request
    /// reloads; the dropped operation itself is never replayed.
    pub(crate) fn note_cancelled(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    async fn reload(&self) -> Result<Instance, HostError> {
        Instance::load(self.package.clone(), self.entrypoint.clone(), &self.cancel)
            .await
            .map_err(|error| HostError::Unavailable(format!("plugin reload: {error}")))
    }

    /// The provider this instance answers under.
    ///
    /// `None` is the harness case: a tool-only extension has no configuration
    /// and no vendor name, so callers on a provider path turn that into their
    /// own error instead of the accessor panicking.
    pub(crate) fn provider(&self) -> Option<&ProviderRole> {
        match &self.role {
            RunningRole::Provider(role) => Some(role),
            RunningRole::Harness => None,
        }
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
