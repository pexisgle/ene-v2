//! MCP connection supervisor: settings/credential diffing, backoff, auth
//! injection, stdio environment/cwd application, and background connection
//! lifecycle management.
use super::projection::{attach, project_servers, retire_one};
use crate::host::{
    db::Store,
    model::Credentials,
    providers::Providers,
    resources::HostRuntime,
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
pub(super) use ene_work::mcp::{
    CheckState, ConnectFailure, Desired, Launch, backoff_delay, is_config_error_message,
};
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use rig_ecs::prelude::*;
use rig_rmcp::McpConnection;
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

/// How many connect results may queue before a producer waits. A connect
/// happens once per server, so this is a bound, not a throughput target.
const QUEUE: usize = 32;

/// One connected server and the tool entities it owns.
pub(super) struct Server {
    pub(super) connection: McpConnection,
    pub(super) launch: Launch,
    pub(super) registry: ToolServerHandle,
    pub(super) label: String,
    pub(super) allowed: Vec<String>,
}

/// The servers that are up right now, keyed by connection id.
#[derive(Resource, Default)]
pub(crate) struct McpServers(pub(super) HashMap<String, Server>);

/// Connection ids a connect task is running for.
#[derive(Resource, Default)]
pub(super) struct Connecting(pub(super) HashMap<String, Pending>);

pub(super) struct Pending {
    pub(super) launch: Launch,
    pub(super) _task: AbortOnDropHandle<()>,
}

pub(super) enum FailureKind {
    Transient {
        attempts: u32,
        next_retry: Instant,
        _wake_timer: Option<AbortOnDropHandle<()>>,
    },
    ConfigHold,
}

pub(super) struct FailureState {
    pub(super) revision: u64,
    pub(super) credential_generation: Option<u64>,
    pub(super) reason: String,
    pub(super) kind: FailureKind,
}

/// Backoff and configuration-hold tracking per failing MCP connection.
#[derive(Resource, Default)]
pub(super) struct McpFailures(pub(super) HashMap<String, FailureState>);

pub(super) enum Event {
    Connected {
        id: String,
        launch: Launch,
        connection: McpConnection,
        registry: ToolServerHandle,
    },
    Failed {
        id: String,
        launch: Launch,
        reason: String,
        config_error: bool,
    },
}

#[derive(Resource)]
pub(super) struct McpEvents {
    pub(super) sender: mpsc::Sender<Event>,
    pub(super) receiver: mpsc::Receiver<Event>,
}

impl Default for McpEvents {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self { sender, receiver }
    }
}

#[derive(Resource, Default)]
pub(super) struct McpSettings {
    pub(super) revision: Option<u64>,
    pub(super) credential_generation: Option<u64>,
    pub(super) desired: Vec<(String, Desired)>,
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
                connection.id.clone(),
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

/// Connects what the settings ask for, retires what they no longer do, and
/// attaches the tools a handler listed.
pub(super) fn sync(world: &mut World) {
    let closing = world
        .get_resource::<crate::host::resources::HostState>()
        .is_some_and(|state| state.closing || state.failed.is_some());
    if closing {
        handle_closing_servers(world);
        return;
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) else {
        return;
    };
    let store = world.resource::<Store>().clone();
    let revision = match store.settings_repo().settings_revision() {
        Ok(revision) => revision,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            return;
        }
    };
    let credentials = world.get_resource::<Credentials>().cloned();
    let reload = {
        let mut settings = world.resource_mut::<McpSettings>();
        reload_desired_settings(&mut settings, &store, credentials.as_ref(), revision)
    };
    match reload {
        ReloadOutcome::Ready => {}
        ReloadOutcome::Skip => return,
        ReloadOutcome::Fail(message) => {
            crate::host::fail(world, message);
            return;
        }
    }
    let wanted = world.resource::<McpSettings>().desired.clone();
    prune_connecting(&mut world.resource_mut::<Connecting>(), &wanted);
    prune_failures(&mut world.resource_mut::<McpFailures>(), &wanted);
    let events = drain(&mut world.resource_mut::<McpEvents>());
    for event in events {
        accept_event(world, &wanted, event, &runtime);
    }
    let retire = collect_retire_ids(world.resource::<McpServers>(), &wanted);
    for id in retire {
        retire_one(world, &id);
    }
    project_servers(world);
    let targets = plan_connect_targets(
        world.resource::<McpServers>(),
        world.resource::<Connecting>(),
        world.resource::<McpFailures>(),
        wanted,
        Instant::now(),
    );
    if !targets.is_empty() {
        let sender = world.resource::<McpEvents>().sender.clone();
        let wake = world.get_resource::<Wake>().cloned();
        let launched = spawn_connect_tasks(targets, &sender, wake, &runtime);
        world.resource_mut::<Connecting>().0.extend(launched);
    }
}

fn handle_closing_servers(world: &mut World) {
    world.resource_mut::<Connecting>().0.clear();
    world.resource_mut::<McpFailures>().0.clear();
    let ids: Vec<String> = world.resource::<McpServers>().0.keys().cloned().collect();
    for id in ids {
        retire_one(world, &id);
    }
    if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
        for event in drain(&mut world.resource_mut::<McpEvents>()) {
            if let Event::Connected { connection, .. } = event {
                runtime.spawn(async move { connection.close().await });
            }
        }
    }
}

enum ReloadOutcome {
    Ready,
    Skip,
    Fail(String),
}

fn reload_desired_settings(
    settings: &mut McpSettings,
    store: &Store,
    credentials: Option<&Credentials>,
    revision: u64,
) -> ReloadOutcome {
    let credential_generation = credentials.map(Credentials::generation);
    let needs_reload = settings.revision != Some(revision)
        || settings.credential_generation != credential_generation;
    if !needs_reload {
        return ReloadOutcome::Ready;
    }
    let desired = match desired(store, credentials, credential_generation) {
        Ok(desired) => desired,
        Err(error) => {
            return if error.is_storage() {
                ReloadOutcome::Fail(error.to_string())
            } else {
                ReloadOutcome::Skip
            };
        }
    };
    *settings = McpSettings {
        revision: Some(revision),
        credential_generation,
        desired,
    };
    ReloadOutcome::Ready
}

fn prune_connecting(connecting: &mut Connecting, wanted: &[(String, Desired)]) {
    // Cancel pending connect tasks whose revision or credentials changed.
    connecting.0.retain(|id, pending| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == pending.launch.revision
                && want.credential_generation == pending.launch.credential_generation
        })
    });
}

fn prune_failures(failures: &mut McpFailures, wanted: &[(String, Desired)]) {
    // Clear failure holds/backoffs when connection config or credentials changed, or connection left wanted.
    failures.0.retain(|id, failure| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == failure.revision
                && want.credential_generation == failure.credential_generation
        })
    });
}

fn collect_retire_ids(servers: &McpServers, wanted: &[(String, Desired)]) -> Vec<String> {
    let wanted_ids: HashSet<&str> = wanted.iter().map(|(id, _)| id.as_str()).collect();
    servers
        .0
        .iter()
        .filter(|(id, server)| {
            server.connection.is_closed()
                || !wanted_ids.contains(id.as_str())
                || wanted
                    .iter()
                    .find(|(wanted, _)| wanted == *id)
                    .is_some_and(|(_, want)| {
                        want.revision != server.launch.revision
                            || want.credential_generation != server.launch.credential_generation
                    })
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn plan_connect_targets(
    servers: &McpServers,
    connecting: &Connecting,
    failures: &McpFailures,
    wanted: Vec<(String, Desired)>,
    now: Instant,
) -> Vec<(String, Desired)> {
    let mut seen = HashSet::new();
    wanted
        .into_iter()
        .filter(|(id, _)| {
            if !seen.insert(id.clone())
                || servers.0.contains_key(id)
                || connecting.0.contains_key(id)
            {
                return false;
            }
            if let Some(failure) = failures.0.get(id) {
                match &failure.kind {
                    FailureKind::ConfigHold => return false,
                    FailureKind::Transient { next_retry, .. } if now < *next_retry => return false,
                    FailureKind::Transient { .. } => {}
                }
            }
            true
        })
        .collect()
}

fn spawn_connect_tasks(
    targets: Vec<(String, Desired)>,
    sender: &mpsc::Sender<Event>,
    wake: Option<Wake>,
    runtime: &tokio::runtime::Handle,
) -> Vec<(String, Pending)> {
    targets
        .into_iter()
        .map(|(id, want)| {
            let registry = ToolServer::new().run();
            let launch = Launch {
                revision: want.revision,
                credential_generation: want.credential_generation,
                owner: registry.owner(),
            };
            let event_id = id.clone();
            let event_launch = launch.clone();
            let sender = sender.clone();
            let wake = wake.clone();
            let task = runtime.spawn(Box::pin(async move {
                let event = match open(&want, registry.clone()).await {
                    Ok(connection) => Event::Connected {
                        id: event_id,
                        launch: event_launch,
                        connection,
                        registry,
                    },
                    Err(failure) => Event::Failed {
                        id: event_id,
                        launch: event_launch,
                        reason: failure.reason,
                        config_error: failure.config_error,
                    },
                };
                match sender.send(event).await {
                    Ok(()) => {
                        if let Some(wake) = wake {
                            wake.wake();
                        }
                    }
                    Err(error) => {
                        if let Event::Connected { connection, .. } = error.0 {
                            connection.close().await;
                        }
                    }
                }
            }));
            (
                id,
                Pending {
                    launch,
                    _task: AbortOnDropHandle::new(task),
                },
            )
        })
        .collect()
}

pub(super) fn accept_event(
    world: &mut World,
    wanted: &[(String, Desired)],
    event: Event,
    runtime: &tokio::runtime::Handle,
) {
    let (id, launch) = match &event {
        Event::Connected { id, launch, .. } | Event::Failed { id, launch, .. } => (id, launch),
    };
    let current = world
        .resource::<Connecting>()
        .0
        .get(id)
        .is_some_and(|pending| &pending.launch == launch)
        && wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == launch.revision
                && want.credential_generation == launch.credential_generation
        });
    if !current {
        if let Event::Connected { connection, .. } = event {
            runtime.spawn(async move { connection.close().await });
        }
        return;
    }
    world.resource_mut::<Connecting>().0.remove(id);
    match event {
        Event::Connected {
            id,
            launch,
            connection,
            registry,
        } => {
            world.resource_mut::<McpFailures>().0.remove(&id);
            let Some((_, want)) = wanted.iter().find(|(wanted, _)| wanted == &id) else {
                return;
            };
            attach(world, &id, launch, connection, registry, want);
        }
        Event::Failed {
            id,
            launch,
            reason,
            config_error,
        } => {
            tracing::warn!(connection = %id, config_error, "the MCP server did not connect: {reason}");
            let wake = world.get_resource::<Wake>().cloned();
            record_failure(
                &mut world.resource_mut::<McpFailures>(),
                FailureReport {
                    id,
                    launch,
                    reason,
                    config_error,
                },
                wake,
                runtime,
            );
        }
    }
}

struct FailureReport {
    id: String,
    launch: Launch,
    reason: String,
    config_error: bool,
}

fn record_failure(
    failures: &mut McpFailures,
    report: FailureReport,
    wake: Option<Wake>,
    runtime: &tokio::runtime::Handle,
) {
    let FailureReport {
        id,
        launch,
        reason,
        config_error,
    } = report;
    let kind = if config_error || is_config_error_message(&reason) {
        FailureKind::ConfigHold
    } else {
        let prev_attempts = match failures.0.get(&id) {
            Some(prev)
                if prev.revision == launch.revision
                    && prev.credential_generation == launch.credential_generation =>
            {
                match prev.kind {
                    FailureKind::Transient { attempts, .. } => attempts,
                    FailureKind::ConfigHold => 0,
                }
            }
            _ => 0,
        };
        let attempts = prev_attempts.saturating_add(1);
        let delay = backoff_delay(attempts);
        let next_retry = Instant::now() + delay;
        let wake_timer = wake.map(|wake| {
            AbortOnDropHandle::new(runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                wake.wake();
            }))
        });
        FailureKind::Transient {
            attempts,
            next_retry,
            _wake_timer: wake_timer,
        }
    };
    failures.0.insert(
        id,
        FailureState {
            revision: launch.revision,
            credential_generation: launch.credential_generation,
            reason,
            kind,
        },
    );
}

fn drain(events: &mut McpEvents) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(event) = events.receiver.try_recv() {
        out.push(event);
    }
    out
}

pub(super) async fn open(
    want: &Desired,
    registry: ToolServerHandle,
) -> Result<McpConnection, ConnectFailure> {
    let client = crate::host::net::client().map_err(|error| ConnectFailure {
        reason: error.to_string(),
        config_error: true,
    })?;
    ene_work::mcp::open(want, registry, client).await
}

/// Clears any backoff or configuration hold on `id` so the supervisor attempts
/// a fresh connection on the next frame (used by `CheckConnection`).
pub(crate) fn request_reconnect(world: &mut World, id: &str) {
    if let Some(mut failures) = world.get_resource_mut::<McpFailures>() {
        failures.0.remove(id);
    }
}

struct CheckContext<'a> {
    cred_gen: Option<u64>,
    servers: Option<&'a McpServers>,
    connecting: Option<&'a Connecting>,
    failures: Option<&'a McpFailures>,
}

/// Inspects the current MCP supervisor state for a specific connection revision
/// and credential generation.
pub(crate) fn check_state(world: &World, id: &str, revision: u64) -> CheckState {
    check_state_from(
        &CheckContext {
            cred_gen: world
                .get_resource::<Credentials>()
                .map(Credentials::generation),
            servers: world.get_resource::<McpServers>(),
            connecting: world.get_resource::<Connecting>(),
            failures: world.get_resource::<McpFailures>(),
        },
        id,
        revision,
    )
}

fn check_state_from(ctx: &CheckContext<'_>, id: &str, revision: u64) -> CheckState {
    if let Some(server) = ctx.servers.and_then(|servers| servers.0.get(id))
        && server.launch.revision == revision
        && server.launch.credential_generation == ctx.cred_gen
        && !server.connection.is_closed()
    {
        let catalog = server.registry.snapshot();
        let mut names: Vec<String> = catalog
            .definitions()
            .iter()
            .map(|def| def.name.as_str().to_owned())
            .collect();
        names.sort();
        names.dedup();
        return CheckState::Connected(names);
    }
    if ctx.connecting.is_some_and(|connecting| {
        connecting
            .0
            .get(id)
            .is_some_and(|pending| pending.launch.revision == revision)
    }) {
        return CheckState::Connecting;
    }
    if let Some(failure) = ctx.failures.and_then(|failures| failures.0.get(id))
        && failure.revision == revision
        && failure.credential_generation == ctx.cred_gen
    {
        return CheckState::Failed(failure.reason.clone());
    }
    CheckState::Connecting
}
