//! Host connection, registration, companion activation bindings, and capability specs.

use super::{Connection, DeviceBridge, Options, Outcome};
use crate::{avatar, capabilities, host::Host};
use buffa::view::MessageView;
use ene_rpc as w;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

pub(super) struct SessionTransport {
    pub(super) path: PathBuf,
    pub(super) options: Options,
    pub(super) connection: Connection,
    /// Companions currently resident (activated) on this client, keyed by companion ID.
    pub(super) bindings: BTreeSet<String>,
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
    pub(super) published_body: bool,
}

impl SessionTransport {
    pub(super) fn new(path: PathBuf, options: Options, connection: Connection) -> Self {
        Self {
            path,
            options,
            connection,
            bindings: BTreeSet::new(),
            focused: None,
            companions: BTreeMap::new(),
            may_activate: true,
            subscription: String::new(),
            cursor: None,
            pending: None,
            published_body: false,
        }
    }

    pub(super) fn focused_companion_id(&self) -> Option<&str> {
        self.focused
            .as_deref()
            .filter(|id| self.bindings.contains(*id))
            .or_else(|| self.bindings.first().map(String::as_str))
    }

    pub(super) fn body_binding_for(&self, companion_id: &str) -> ene_domain::BodyBinding {
        self.companions
            .get(companion_id)
            .and_then(|companion| companion.body.as_option())
            .and_then(|doc| serde_json::from_str::<ene_domain::BodyBinding>(&doc.json).ok())
            .unwrap_or_default()
    }

    pub(super) fn resolve_model(&self, body: &ene_domain::BodyBinding) -> Option<PathBuf> {
        avatar::resolve_model_path(body.model.as_deref(), self.options.model.as_deref())
    }

    pub(super) fn set_connection(&self, host: Option<Host>) {
        *self.connection.write() = host;
    }

    pub(super) fn current_host(&self) -> Option<Host> {
        self.connection.read().clone()
    }

    pub(super) async fn sync_capabilities(&mut self, host: &Host, devices: &DeviceBridge) {
        if let Err(error) = self.register(host, devices).await {
            tracing::warn!("failed to sync client capabilities in place: {error}");
        }
    }

    /// Registers this client, keeping the host's grants across restarts.
    pub(super) async fn register(
        &mut self,
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
        let body = devices.body_available(self);
        let mut declaration = capabilities::Declaration::probe(body);
        declaration.eyes &= devices.eyes_enabled;
        let mut specs = declaration.specs();
        if let Some(workspace) = &devices.workspace {
            specs.extend(workspace.specs());
        }
        specs.extend(devices.client_plugins.specs().iter().cloned());
        specs.extend(computer_spec);
        let client = host
            .client
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
            .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))?;
        self.published_body = body;
        Ok(client)
    }

    /// Installs the host snapshot and the cursor at which it was taken.
    pub(super) fn restore_snapshot(&mut self, snapshot: &w::Snapshot) -> BTreeSet<String> {
        self.companions = snapshot
            .companions
            .iter()
            .map(|companion| (companion.id.clone(), companion.clone()))
            .collect();
        let previous = std::mem::replace(
            &mut self.bindings,
            snapshot
                .activations
                .iter()
                .filter(|activation| {
                    activation.client_id.as_deref() == Some(self.options.id.as_str())
                })
                .filter(|activation| self.companions.contains_key(&activation.companion_id))
                .map(|activation| activation.companion_id.clone())
                .collect(),
        );
        if self
            .focused
            .as_deref()
            .is_none_or(|id| !self.bindings.contains(id))
        {
            self.focused = snapshot
                .active_companion_id
                .as_deref()
                .filter(|id| self.bindings.contains(*id))
                .map(str::to_owned)
                .or_else(|| self.bindings.first().cloned());
        }
        self.cursor = snapshot.cursor.as_option().cloned();
        previous
    }

    /// Binds this client to the host state and applies pending activation intent.
    pub(super) async fn bind(
        &mut self,
        host: &Host,
        snapshot: &w::Snapshot,
    ) -> Result<bool, Outcome> {
        let previous = self.restore_snapshot(snapshot);
        if !self.bindings.is_empty() {
            // Re-attaching to an existing host activation must consume initial
            // activation intent so a future move away never auto-steals back (B16).
            self.may_activate = false;
            if let Some(target) = self.pending.take() {
                if !self.bindings.contains(&target) && self.companions.contains_key(&target) {
                    self.activate_one(host, &target)
                        .await
                        .map_err(|error| self.classify(&error))?;
                    self.bindings.insert(target.clone());
                }
                if self.bindings.contains(&target) {
                    self.focused = Some(target);
                }
            }
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
            Ok(()) => {
                self.bindings.insert(wanted.clone());
                self.focused = Some(wanted);
                self.may_activate = false;
                self.pending = None;
                Ok(previous != self.bindings)
            }
            Err(error) => Err(self.classify(&error)),
        }
    }

    /// Activates `companion_id` onto this client without releasing other
    /// companions already resident here.
    pub(super) async fn activate_one(
        &self,
        host: &Host,
        companion_id: &str,
    ) -> Result<(), connectrpc::ConnectError> {
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
        Ok(())
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
    pub(super) fn classify(&self, error: &connectrpc::ConnectError) -> Outcome {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_snapshot_changes_residents_and_cursor_together() {
        let mut transport =
            SessionTransport::new(PathBuf::new(), Options::default(), Connection::default());
        let snapshot = |epoch: &str, seq, resident: &str, other: &str| w::Snapshot {
            active_companion_id: Some(resident.into()),
            cursor: Some(w::Cursor {
                epoch: epoch.into(),
                seq,
                ..Default::default()
            })
            .into(),
            companions: [resident, other]
                .into_iter()
                .map(|id| w::Companion {
                    id: id.into(),
                    ..Default::default()
                })
                .collect(),
            activations: [
                (resident, "desktop"),
                (other, "phone"),
                ("absent", "desktop"),
            ]
            .into_iter()
            .map(|(id, client)| w::Activation {
                companion_id: id.into(),
                client_id: Some(client.into()),
                ..Default::default()
            })
            .collect(),
            ..Default::default()
        };
        let first = snapshot("boot-a", 9, "ene", "kai");
        assert_eq!(transport.restore_snapshot(&first), BTreeSet::new());
        assert_eq!(transport.focused_companion_id(), Some("ene"));
        assert_eq!(transport.cursor.as_ref(), first.cursor.as_option());

        let replacement = snapshot("boot-b", 1, "kai", "ene");
        assert_eq!(
            transport.restore_snapshot(&replacement),
            BTreeSet::from(["ene".to_owned()])
        );
        assert_eq!(transport.bindings, BTreeSet::from(["kai".to_owned()]));
        assert_eq!(transport.focused_companion_id(), Some("kai"));
        assert_eq!(transport.cursor.as_ref(), replacement.cursor.as_option());
    }
}
