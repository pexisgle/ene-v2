//! Host connection, registration, companion activation bindings, and capability specs.

use super::{Binding, Connection, DeviceBridge, Options, Outcome};
use crate::{avatar, capabilities, host::Host, plugins::ClientPlugins, work_tools::Workspace};
use buffa::view::MessageView;
use ene_rpc as w;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub(super) struct SessionTransport {
    pub(super) path: PathBuf,
    pub(super) options: Options,
    pub(super) connection: Connection,
    /// Companions currently resident (activated) on this client, keyed by companion ID.
    pub(super) bindings: BTreeMap<String, Binding>,
    /// Companion currently focused by the pod / interactive controls.
    pub(super) focused: Option<String>,
    /// Latest companion records from the host, used to resolve body/model per resident companion.
    pub(super) companions: BTreeMap<String, w::Companion>,
    /// Whether the next attempt may move the activation. It is set on the first
    /// connect and when the operator picks a companion, so a reconnect never
    /// steals the companion back from another device.
    pub(super) may_activate: bool,
    /// The surface of the current device stream, from `DeviceReady`.
    pub(super) subscription: String,
    /// The last notification cursor, for a resuming subscription.
    pub(super) cursor: Option<w::Cursor>,
    /// The companion the operator asked to serve while a request was in flight.
    pub(super) pending: Option<String>,
    /// Capabilities this machine can honour, declared at registration.
    pub(super) specs: Vec<w::ClientCapabilitySpec>,
}

impl SessionTransport {
    pub(super) fn new(
        path: PathBuf,
        options: Options,
        connection: Connection,
        workspace: Option<&Arc<Workspace>>,
        client_plugins: &Arc<ClientPlugins>,
    ) -> Self {
        let mut specs =
            capabilities::Declaration::probe(options.renderer.as_deref(), options.model.as_deref())
                .specs();
        if let Some(workspace) = workspace {
            specs.extend(workspace.specs());
        }
        specs.extend(client_plugins.specs().iter().cloned());
        Self {
            path,
            options,
            connection,
            bindings: BTreeMap::new(),
            focused: None,
            companions: BTreeMap::new(),
            may_activate: true,
            subscription: String::new(),
            cursor: None,
            pending: None,
            specs,
        }
    }

    pub(super) fn focused_companion_id(&self) -> Option<&str> {
        self.focused
            .as_deref()
            .filter(|id| self.bindings.contains_key(*id))
            .or_else(|| self.bindings.keys().next().map(String::as_str))
    }

    pub(super) fn focused_binding(&self) -> Option<&Binding> {
        self.focused_companion_id()
            .and_then(|id| self.bindings.get(id))
    }

    pub(super) fn body_binding_for(&self, companion_id: &str) -> ene_domain::BodyBinding {
        self.companions
            .get(companion_id)
            .and_then(|companion| companion.body.as_option())
            .and_then(|doc| serde_json::from_str::<ene_domain::BodyBinding>(&doc.json).ok())
            .unwrap_or_default()
    }

    pub(super) fn model_for(&self, companion_id: &str) -> Option<PathBuf> {
        let body = self.body_binding_for(companion_id);
        avatar::resolve_model_path(body.model.as_deref(), self.options.model.as_deref())
    }

    pub(super) fn set_connection(&self, host: Option<Host>) {
        *self.connection.write() = host;
    }

    pub(super) fn current_host(&self) -> Option<Host> {
        self.connection.read().clone()
    }

    pub(super) fn rebuild_specs(
        &mut self,
        workspace: Option<&Arc<Workspace>>,
        client_plugins: &Arc<ClientPlugins>,
    ) {
        let mut specs = capabilities::Declaration::probe(
            self.options.renderer.as_deref(),
            self.options.model.as_deref(),
        )
        .specs();
        if let Some(workspace) = workspace {
            specs.extend(workspace.specs());
        }
        specs.extend(client_plugins.specs().iter().cloned());
        self.specs = specs;
    }

    pub(super) async fn sync_capabilities(&mut self, host: &Host, devices: &DeviceBridge) {
        self.rebuild_specs(devices.workspace.as_ref(), &devices.client_plugins);
        if let Err(error) = self.register(host, devices).await {
            tracing::warn!("failed to sync client capabilities in place: {error}");
        }
    }

    /// Registers this client, keeping the host's grants across restarts.
    pub(super) async fn register(
        &self,
        host: &Host,
        devices: &DeviceBridge,
    ) -> Result<w::Client, connectrpc::ConnectError> {
        // A declaration is an optimistic update, so a restart must send the
        // revision the host already stored or it conflicts with itself.
        let expected_revision = host
            .client
            .list(w::Empty::default())
            .await?
            .view()
            .to_owned_message()
            .ok()
            .and_then(|clients| {
                clients
                    .items
                    .iter()
                    .find(|client| client.id == self.options.id)
                    .map(|client| client.revision)
            })
            .unwrap_or(0);
        let computer_spec = {
            #[cfg(windows)]
            {
                devices.computer_enabled.then(crate::computer::spec)
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        let specs: Vec<_> = self
            .specs
            .iter()
            .filter(|spec| devices.eyes_enabled || spec.name != "eye.watch")
            .cloned()
            .chain(computer_spec)
            .collect();
        host.client
            .register_client(w::RegisterClientRequest {
                id: self.options.id.clone(),
                name: self.options.name.clone(),
                platform: self.options.platform.clone(),
                capabilities: capabilities::names(&specs),
                capability_specs: specs,
                expected_revision,
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))
    }

    /// Binds this client to its resident companions from `snapshot` without
    /// stealing companions already held by other devices. Returns whether
    /// resident bindings changed.
    pub(super) async fn bind(
        &mut self,
        host: &Host,
        snapshot: &w::Snapshot,
    ) -> Result<bool, Outcome> {
        let previous = self.bindings.clone();
        self.companions = snapshot
            .companions
            .iter()
            .map(|companion| (companion.id.clone(), companion.clone()))
            .collect();
        self.bindings = snapshot
            .activations
            .iter()
            .filter(|activation| activation.client_id.as_deref() == Some(self.options.id.as_str()))
            .map(|activation| {
                (
                    activation.companion_id.clone(),
                    Binding {
                        companion_id: activation.companion_id.clone(),
                    },
                )
            })
            .collect();

        if !self.bindings.is_empty() {
            // Re-attaching to an existing host activation must consume initial
            // activation intent so a future move away never auto-steals back (B16).
            self.may_activate = false;
            self.rebind_existing(host, snapshot).await?;
            return Ok(previous != self.bindings);
        }

        let wanted = self
            .pending
            .clone()
            .or_else(|| snapshot.active_companion_id.clone())
            .or_else(|| {
                snapshot
                    .companions
                    .first()
                    .map(|companion| companion.id.clone())
            });
        let Some(wanted) = wanted else {
            self.bindings.clear();
            self.focused = None;
            return Ok(previous != self.bindings);
        };
        if !self.may_activate {
            // Another device holds this companion. Serving nothing is the
            // honest state: no audio, no perception, no device results.
            self.bindings.clear();
            self.focused = None;
            self.pending = None;
            tracing::info!("another client is the active device for {wanted}");
            return Ok(previous != self.bindings);
        }
        match self.activate_one(host, &wanted).await {
            Ok(binding) => {
                self.bindings.insert(wanted.clone(), binding);
                self.focused = Some(wanted);
                self.may_activate = false;
                self.pending = None;
                Ok(previous != self.bindings)
            }
            Err(error) => Err(self.classify(error)),
        }
    }

    async fn rebind_existing(
        &mut self,
        host: &Host,
        snapshot: &w::Snapshot,
    ) -> Result<(), Outcome> {
        if let Some(target) = self.pending.take() {
            if !self.bindings.contains_key(&target) && self.companions.contains_key(&target) {
                match self.activate_one(host, &target).await {
                    Ok(binding) => {
                        self.bindings.insert(target.clone(), binding);
                    }
                    Err(error) => return Err(self.classify(error)),
                }
            }
            if self.bindings.contains_key(&target) {
                self.focused = Some(target);
            }
        }
        if self
            .focused
            .as_deref()
            .is_none_or(|id| !self.bindings.contains_key(id))
        {
            self.focused = snapshot
                .active_companion_id
                .as_deref()
                .filter(|id| self.bindings.contains_key(*id))
                .map(str::to_owned)
                .or_else(|| self.bindings.keys().next().cloned());
        }
        Ok(())
    }

    /// Activates `companion_id` onto this client without releasing other
    /// companions already resident here.
    pub(super) async fn activate_one(
        &self,
        host: &Host,
        companion_id: &str,
    ) -> Result<Binding, connectrpc::ConnectError> {
        let activation = host
            .client
            .activate(w::ActivationRequest {
                companion_id: companion_id.to_owned(),
                client_id: self.options.id.clone(),
                release: false,
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))?;
        tracing::info!(
            companion = %activation.companion_id,
            "the client is an active device"
        );
        Ok(Binding {
            companion_id: activation.companion_id,
        })
    }

    /// Releases `companion_id` from this client.
    pub(super) async fn release_one(
        &self,
        host: &Host,
        companion_id: &str,
    ) -> Result<(), connectrpc::ConnectError> {
        host.client
            .activate(w::ActivationRequest {
                companion_id: companion_id.to_owned(),
                client_id: String::new(),
                release: true,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Turns an RPC error into what the window must show.
    pub(super) fn classify(&self, error: connectrpc::ConnectError) -> Outcome {
        match error.code {
            // The endpoint or token is stale: the next attempt re-reads the file.
            connectrpc::ErrorCode::Unauthenticated | connectrpc::ErrorCode::PermissionDenied => {
                Outcome::Dropped(format!(
                    "認証が拒否されました。ホストの接続情報を読み直します: {error}"
                ))
            }
            _ => Outcome::Dropped(error.to_string()),
        }
    }

    pub(super) fn perception_event(&self, event: w::perception_event::Event) -> w::PerceptionEvent {
        w::PerceptionEvent {
            client_id: self.options.id.clone(),
            subscription_id: self.subscription.clone(),
            event: Some(event),
            ..Default::default()
        }
    }
}
