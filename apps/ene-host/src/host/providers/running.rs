//! One admitted plugin instance and its invocation lifetime.
use super::Loaded;
use crate::host::error::HostError;
use ene_plugin::{Extension, ProviderConfiguration};
use ene_plugin_runtime::instance::{Instance, Services};
use rig_core::providers::registry::ProviderId;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::runtime::Handle;
use tokio_util::context::TokioContext;
use tokio_util::sync::CancellationToken;

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
    provider: Option<Box<ProviderRole>>,
    instance: tokio::sync::Mutex<Option<Instance>>,
    loaded: Arc<Loaded>,
    entrypoint: String,
    runtime: Handle,
    cancel: CancellationToken,
    /// A caller dropped a call before it reported. The next request releases
    /// the dead instance and reloads; a call that failed on its own keeps the
    /// old policy and is not restarted automatically.
    cancelled: AtomicBool,
}

impl Running {
    /// Starts the entrypoint that serves `extension`.
    pub(super) async fn start(
        loaded: Arc<Loaded>,
        extension: Extension,
        cancel: CancellationToken,
    ) -> Result<Self, HostError> {
        let runtime = Handle::try_current()
            .map_err(|error| HostError::Unavailable(format!("plugin runtime: {error}")))?;
        let (entrypoint, provider) = if !extension.serves_provider() {
            let entrypoint = loaded.extension_entrypoint(extension).ok_or_else(|| {
                HostError::Unavailable(format!(
                    "the plugin `{}` serves no {extension:?} entrypoint",
                    loaded.package.manifest.id
                ))
            })?;
            (entrypoint.to_owned(), None)
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
                Some(Box::new(ProviderRole {
                    configuration: provider.clone(),
                    id: provider_id,
                })),
            )
        };
        // `Instance::load` runs the guest's initialization hook, so this is the
        // one place `ene_initialize` is called for an instance.
        let instance = Instance::load(loaded.package.clone(), entrypoint.clone(), &cancel)
            .await
            .map_err(|error| HostError::Unavailable(format!("plugin load: {error}")))?;
        Ok(Self {
            provider,
            instance: tokio::sync::Mutex::new(Some(instance)),
            loaded,
            entrypoint,
            runtime,
            cancel,
            cancelled: AtomicBool::new(false),
        })
    }

    /// Runs one operation on the plugin and returns its reply document.
    pub(crate) async fn invoke(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn Services>,
    ) -> Result<serde_json::Value, HostError> {
        let (sender, mut events) = tokio::sync::mpsc::channel(16);
        // A caller that wants no stream still must drain it, or a plugin that
        // emits would block on a full channel.
        self.runtime
            .spawn(async move { while events.recv().await.is_some() {} });
        self.invoke_streaming(operation, services, sender).await
    }

    /// Runs one operation and hands its events to `events` as they arrive, so
    /// a streamed answer reaches the caller while the plugin is still
    /// producing it.
    pub(crate) async fn invoke_streaming(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn Services>,
        events: tokio::sync::mpsc::Sender<ene_plugin::Event>,
    ) -> Result<serde_json::Value, HostError> {
        TokioContext::new(self.call(operation, services, events), self.runtime.clone()).await
    }

    async fn call(
        &self,
        operation: ene_plugin::Operation,
        services: Arc<dyn Services>,
        events: tokio::sync::mpsc::Sender<ene_plugin::Event>,
    ) -> Result<serde_json::Value, HostError> {
        let mut slot = self
            .cancel
            .run_until_cancelled(self.instance.lock())
            .await
            .ok_or_else(|| HostError::Unavailable("the plugin was disabled".into()))?;
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

    pub(crate) fn revoked(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub(crate) fn package_revision(&self) -> &str {
        &self.loaded.package.revision
    }

    async fn reload(&self) -> Result<Instance, HostError> {
        Instance::load(
            self.loaded.package.clone(),
            self.entrypoint.clone(),
            &self.cancel,
        )
        .await
        .map_err(|error| HostError::Unavailable(format!("plugin reload: {error}")))
    }

    /// The provider this instance answers under.
    ///
    /// `None` is the harness case: a tool-only extension has no configuration
    /// and no vendor name, so callers on a provider path turn that into their
    /// own error instead of the accessor panicking.
    pub(crate) fn provider(&self) -> Option<&ProviderRole> {
        self.provider.as_deref()
    }
}
