//! The snapshot the desktop takes at startup and after every resync.

use super::*;

pub(crate) struct SnapshotReader<'a> {
    pub(crate) store: &'a Store,
    pub(crate) health: &'a HostState,
    pub(crate) notifier: &'a Notifier,
    pub(crate) hub: &'a DeviceHub,
    pub(crate) providers: &'a providers::Providers,
}

pub(crate) struct Subscription {
    pub(crate) cursor: w::Cursor,
    pub(crate) backlog: Vec<Arc<w::Notification>>,
    pub(crate) live: broadcast::Receiver<Arc<w::Notification>>,
}

impl<'a> From<&'a HostServices> for SnapshotReader<'a> {
    fn from(services: &'a HostServices) -> Self {
        Self {
            store: &services.store,
            health: &services.health,
            notifier: &services.notifier,
            hub: &services.hub,
            providers: &services.providers,
        }
    }
}

impl SnapshotReader<'_> {
    pub(crate) fn get(&self) -> Result<w::Snapshot> {
        let Self {
            store,
            health,
            notifier,
            hub,
            providers,
        } = *self;
        let companions: Vec<w::Companion> = store
            .companion_repo()
            .companions()?
            .iter()
            .map(wire::companion)
            .collect();
        let activations = store.client_repo().activations()?;
        let active_companion_id = activations
            .iter()
            .find(|activation| activation.client_id.is_some())
            .map(|activation| activation.companion_id.as_str().to_owned());
        // One query, then grouped back into the per-companion order this loop used
        // to produce: companions in snapshot order, each companion's sessions
        // newest first. The wire output stays identical, and a host with many
        // companions no longer pays a round trip for each.
        let mut by_companion: std::collections::HashMap<String, Vec<_>> =
            std::collections::HashMap::new();
        for session in store.work_repo().all_work_sessions()? {
            by_companion
                .entry(session.companion_id.as_str().to_owned())
                .or_default()
                .push(session);
        }
        let work_sessions = companions
            .iter()
            .filter_map(|companion| by_companion.remove(&companion.id))
            .flatten()
            .map(|session| w::WorkSession::from(&session))
            .collect();
        let clients = client::clients_with_activations(store, hub, &activations)?;
        Ok(w::Snapshot {
            server_version: env!("CARGO_PKG_VERSION").into(),
            health: w::HostHealth::from(health.health()).into(),
            capabilities: w::HostCapabilities {
                limits: Some(w::JsonDocument {
                    schema: "ene.limits.v1".into(),
                    json: serde_json::json!({
                        "max_input_tokens": chat::MAX_INPUT_TOKENS,
                        "max_input_chars": chat::MAX_INPUT_CHARS,
                        "max_companions": MAX_COMPANIONS,
                    })
                    .to_string(),
                    ..Default::default()
                })
                .into(),
                features: [
                    "history",
                    "notifications",
                    "chat",
                    "connections",
                    "credentials",
                    "mind",
                    "work",
                    "clients",
                    "device-stream",
                    "perception",
                    "diagnostics",
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
                adapters: settings::providers(providers)
                    .into_iter()
                    .map(|provider| provider.adapter)
                    .collect(),
                transport: "/rpc".into(),
                ..Default::default()
            }
            .into(),
            settings: Some(settings::project(
                &store.settings_repo().settings()?,
                providers,
            ))
            .into(),
            companions,
            work_sessions,
            active_companion_id,
            pending_approvals: store
                .approval_repo()
                .approvals(permission::ApprovalStatus::Pending)?
                .iter()
                .map(wire::approval)
                .collect(),
            clients,
            cursor: Some(notifier.cursor()).into(),
            policy_rules: store
                .policy_repo()
                .policy_rules()?
                .iter()
                .map(Into::into)
                .collect(),
            activations: activations.iter().map(Into::into).collect(),
            ..Default::default()
        })
    }

    pub(crate) fn subscribe(
        &self,
        client_id: Option<&str>,
        cursor: Option<&w::Cursor>,
    ) -> Result<Subscription> {
        let store = self.store;
        if let Some(client_id) = client_id.filter(|id| !id.is_empty()) {
            ene_domain::validate_id(client_id)?;
            if store.client_repo().client(client_id)?.is_none() {
                return Err(HostError::NotFound("client not found".into()));
            }
        }
        let notifier = self.notifier;
        let live = notifier.subscribe();
        let (cursor_at_start, backlog) = match cursor.and_then(|cursor| notifier.replay(cursor)) {
            Some(events) => (notifier.cursor(), events),
            None => {
                let snapshot = self.get()?;
                let initial = Arc::new(w::Notification {
                    cursor: snapshot.cursor.clone(),
                    kind: "initial_state".into(),
                    event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
                    ..Default::default()
                });
                (notifier.cursor(), vec![initial])
            }
        };
        Ok(Subscription {
            cursor: cursor_at_start,
            backlog,
            live,
        })
    }
}
