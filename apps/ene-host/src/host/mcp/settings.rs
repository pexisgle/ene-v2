//! The enabled MCP connection configurations the supervisor reconciles.
use super::supervisor::{Connecting, McpServers};
use crate::host::{db::Store, model::Credentials};
use ene_domain::settings::{ConnectionConfig, CredentialState, McpConnection};
use ene_work::mcp::ConnectFailure;
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
    pub(super) secret: Result<Option<String>, ConnectFailure>,
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
            let secret = credentials
                .map_or(Ok(None), |keys| keys.secret(id))
                .map_err(|_| {
                    ConnectFailure::Transient("MCP credential store could not be read".into())
                })
                .and_then(|key| {
                    if key.is_none() && connection.credential_state == CredentialState::Registered {
                        return Err(ConnectFailure::Configuration(
                            "the registered MCP credential is unavailable".into(),
                        ));
                    }
                    Ok(key)
                });
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
                    secret,
                },
            ))
        })
        .collect())
}

pub(super) fn prune_connecting(connecting: &mut Connecting, wanted: &DesiredServers) {
    connecting.0.retain(|id, pending| {
        wanted
            .get(id)
            .is_some_and(|want| want.stamp == pending.launch.stamp)
    });
}

pub(super) fn collect_retire_ids(servers: &McpServers, wanted: &DesiredServers) -> Vec<String> {
    servers
        .0
        .iter()
        .filter(|(id, server)| {
            server.connection.is_closed()
                || !wanted
                    .get(*id)
                    .is_some_and(|want| want.stamp == server.launch.stamp)
        })
        .map(|(id, _)| id.clone())
        .collect()
}
