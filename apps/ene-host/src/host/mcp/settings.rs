//! The settings snapshot the supervisor diffs against: which MCP connections
//! the operator wants, and how they translate into `Desired` launches.
use super::supervisor::{Connecting, McpServers};
use crate::host::{
    db::Store,
    model::Credentials,
    providers::Providers,
    settings::{ConnectionConfig, ConnectionKind},
};
use ene_work::mcp::Desired;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct McpSettings {
    pub(super) revision: Option<u64>,
    pub(super) credential_generation: Option<u64>,
    pub(super) desired: Arc<[(String, Desired)]>,
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
        settings.desired = desired(store, credentials, credential_generation)?.into();
        settings.revision = Some(revision);
        settings.credential_generation = credential_generation;
    }
    Ok(())
}

fn desired(
    store: &Store,
    credentials: Option<&Credentials>,
    credential_generation: Option<u64>,
) -> crate::host::db::Result<Vec<(String, Desired)>> {
    let settings = store.settings_repo().settings()?;
    let no_providers = Providers::none();
    Ok(settings
        .connections
        .iter()
        .filter(|connection| connection.enabled && connection.kind == ConnectionKind::Mcp)
        .filter_map(|connection| {
            let ConnectionConfig::Mcp(config) = &connection.config else {
                return None;
            };
            let secret =
                crate::host::model::connection_secret(connection, credentials, &no_providers)
                    .ok()
                    .flatten();
            Some((
                connection.id.as_str().to_owned(),
                Desired {
                    revision: connection.revision,
                    credential_generation,
                    name: connection.name.clone(),
                    command: config.command.trim().to_owned(),
                    args: config.args.clone(),
                    cwd: config.cwd.trim().to_owned(),
                    env: config.env.clone(),
                    headers: config.headers.clone(),
                    auth_header: config.auth_header.trim().to_owned(),
                    secret_env_var: config.secret_env_var.trim().to_owned(),
                    endpoint: config.endpoint.endpoint.trim().to_owned(),
                    secret,
                    allowed: config.allowed_tools.clone(),
                },
            ))
        })
        .collect())
}

pub(super) fn prune_connecting(connecting: &mut Connecting, wanted: &[(String, Desired)]) {
    // Cancel pending connect tasks whose revision or credentials changed.
    connecting.0.retain(|id, pending| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == pending.launch.revision
                && want.credential_generation == pending.launch.credential_generation
        })
    });
}

pub(super) fn collect_retire_ids(
    servers: &McpServers,
    wanted: &[(String, Desired)],
) -> Vec<String> {
    servers
        .0
        .iter()
        .filter(|(id, server)| {
            server.connection.is_closed()
                || !wanted.iter().any(|(wanted, want)| {
                    wanted == *id
                        && want.revision == server.launch.revision
                        && want.credential_generation == server.launch.credential_generation
                })
        })
        .map(|(id, _)| id.clone())
        .collect()
}
