//! The enabled MCP connection configurations the supervisor reconciles.
use crate::host::{db::Store, model::Credentials};
use ene_domain::settings::{ConnectionConfig, CredentialState, McpConnection};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionStamp {
    pub(crate) revision: u64,
    pub(crate) created_revision: u64,
    pub(crate) credential_generation: u64,
}

impl ConnectionStamp {
    pub(crate) fn current(
        self,
        store: &Store,
        keys: Option<&Credentials>,
        id: &str,
    ) -> crate::host::db::Result<bool> {
        Ok(
            keys.map_or(0, |keys| keys.generation_of(id)) == self.credential_generation
                && store
                    .settings_repo()
                    .connection_with_creation(id)?
                    .is_some_and(|(row, created)| {
                        row.enabled
                            && row.revision == self.revision
                            && created == self.created_revision
                    }),
        )
    }
}

#[derive(Clone)]
pub(super) struct Desired {
    pub(super) stamp: ConnectionStamp,
    pub(super) name: String,
    pub(super) config: McpConnection,
    pub(super) credential_state: CredentialState,
}

pub(super) type DesiredServers = BTreeMap<String, Desired>;

#[derive(Default)]
pub(super) struct McpSettings {
    revision: Option<u64>,
    credential_generation: Option<u64>,
    pub(super) desired: Arc<DesiredServers>,
}

pub(super) fn reload_desired_settings(
    settings: &mut McpSettings,
    store: &Store,
    credentials: Option<&Credentials>,
) -> crate::host::db::Result<()> {
    let revision = store.settings_repo().settings_revision()?;
    let credential_generation = credentials.map(Credentials::generation);
    if settings.revision != Some(revision)
        || settings.credential_generation != credential_generation
    {
        settings.desired = Arc::new(desired(store, credentials)?);
        settings.revision = Some(revision);
        settings.credential_generation = credential_generation;
    }
    Ok(())
}

fn desired(
    store: &Store,
    credentials: Option<&Credentials>,
) -> crate::host::db::Result<DesiredServers> {
    Ok(store
        .settings_repo()
        .enabled_mcp_connections()?
        .into_iter()
        .filter_map(|(connection, created_revision)| {
            let ConnectionConfig::Mcp(config) = connection.config else {
                return None;
            };
            let id = connection.id.as_str();
            Some((
                id.to_owned(),
                Desired {
                    stamp: ConnectionStamp {
                        revision: connection.revision,
                        created_revision,
                        credential_generation: credentials.map_or(0, |keys| keys.generation_of(id)),
                    },
                    name: connection.name,
                    config,
                    credential_state: connection.credential_state,
                },
            ))
        })
        .collect())
}
