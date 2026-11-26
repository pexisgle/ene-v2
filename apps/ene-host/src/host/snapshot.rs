//! The snapshot the desktop takes at startup and after every resync.

use super::*;

pub(crate) fn snapshot(app: &mut App) -> Result<w::Snapshot> {
    let store = app.world().resource::<Store>().clone();
    let state = app.world().resource::<HostState>();
    let notifier = app.world().resource::<Notifier>();
    let hub = app.world().resource::<DeviceHub>();
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
    let mut work_sessions = Vec::new();
    for companion in &companions {
        for session in by_companion.remove(&companion.id).unwrap_or_default() {
            work_sessions.push(w::WorkSession::from(&session));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let work_sessions = work_sessions
        .into_iter()
        .filter(|session| seen.insert(session.id.clone()))
        .collect();
    let clients = client::clients(&store, hub)?;
    let loaded = app
        .world()
        .get_resource::<providers::Providers>()
        .cloned()
        .unwrap_or_default();
    Ok(w::Snapshot {
        server_version: env!("CARGO_PKG_VERSION").into(),
        health: w::HostHealth {
            ready: state.ready().is_ok(),
            recovery_required: state.failed.is_some(),
            stopping: state.closing,
            ..Default::default()
        }
        .into(),
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
            adapters: settings::providers(&loaded)
                .into_iter()
                .map(|provider| provider.adapter)
                .collect(),
            transport: "/rpc".into(),
            ..Default::default()
        }
        .into(),
        settings: Some(wire::settings(&store.settings_repo().settings()?)).into(),
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
