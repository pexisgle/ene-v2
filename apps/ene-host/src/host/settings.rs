//! Provider connections and settings revision.
use super::{
    db::{Result, Store, new_id, next_revision, now, validate_id},
    error::HostError,
    model,
    notify::Notifier,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialState {
    Missing,
    Registered,
}

impl CredentialState {
    pub(crate) fn parse(value: &str) -> Self {
        if value == "registered" {
            Self::Registered
        } else {
            Self::Missing
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Registered => "registered",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Connection {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    pub(crate) options: Value,
    pub(crate) credential_ref: Option<String>,
    pub(crate) credential_state: CredentialState,
    pub(crate) last_check: Value,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) revision: u64,
    pub(crate) connections: Vec<Connection>,
    pub(crate) updated_at: String,
}

pub(crate) fn settings(store: &Store) -> Result<w::Settings> {
    let settings = store.settings()?;
    Ok(w::Settings {
        revision: settings.revision,
        connections: settings.connections.iter().map(wire::connection).collect(),
        updated_at: settings.updated_at,
        ..Default::default()
    })
}

pub(crate) fn update_settings(
    world: &mut World,
    request: w::UpdateSettingsRequest,
) -> Result<(w::Settings, Vec<String>)> {
    let store = world.resource::<Store>().clone();
    let current = store.settings()?;
    if current.revision != request.expected_revision {
        return Err(HostError::conflict("settings revision changed"));
    }
    let mut existing = current.connections.clone();
    let mut next = Vec::new();
    for input in &request.connections {
        let id = if input.id.is_empty() {
            new_id()
        } else {
            input.id.clone()
        };
        validate_id(&id)?;
        if input.adapter.is_empty() {
            return Err(HostError::invalid("adapter is required"));
        }
        let endpoint = input.endpoint.trim().trim_end_matches('/').to_owned();
        let options = wire::parse(&input.options)?;
        super::model::validate_connection(&input.adapter, &input.kind, &endpoint, &options)?;
        let found = existing
            .iter()
            .position(|connection| connection.id == id)
            .map(|index| existing.remove(index));
        let connection = match found {
            Some(mut found) => {
                found.adapter = input.adapter.clone();
                found.kind = input.kind.clone();
                found.endpoint = endpoint;
                found.options = options;
                found.enabled = input.enabled;
                found
            }
            None => Connection {
                id,
                kind: input.kind.clone(),
                adapter: input.adapter.clone(),
                endpoint,
                options,
                credential_ref: None,
                credential_state: CredentialState::Missing,
                last_check: json!({}),
                enabled: input.enabled,
            },
        };
        next.push(connection);
    }
    let removed: Vec<String> = existing
        .into_iter()
        .map(|connection| connection.id)
        .collect();
    let forgotten = current
        .connections
        .iter()
        .filter(|connection| removed.contains(&connection.id))
        .filter_map(|connection| connection.credential_ref.clone())
        .collect();
    let revision = next_revision(current.revision)?;
    store.replace_connections(revision, &now(), &next, &removed)?;
    let view = settings(&store)?;
    notify_settings(world, &view);
    Ok((view, forgotten))
}

fn notify_settings(world: &mut World, view: &w::Settings) {
    world.resource_mut::<Notifier>().push(
        None,
        None,
        w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
            settings: view.clone().into(),
            ..Default::default()
        })),
    );
}

pub(crate) fn credential_changed(world: &mut World, connection_id: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store.bump_settings()?;
    model::invalidate_connection(world, connection_id);
    let view = settings(&store)?;
    notify_settings(world, &view);
    Ok(())
}
