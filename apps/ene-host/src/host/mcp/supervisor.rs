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
#[derive(Default)]
pub(crate) struct McpServers(pub(super) HashMap<String, Server>);

/// Connection ids a connect task is running for.
#[derive(Default)]
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
#[derive(Default)]
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

/// The event channel the connect tasks report on.
pub(super) struct EventChannel {
    pub(super) sender: mpsc::Sender<Event>,
    pub(super) receiver: mpsc::Receiver<Event>,
}

impl Default for EventChannel {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE);
        Self { sender, receiver }
    }
}

#[derive(Default)]
pub(super) struct McpSettings {
    pub(super) revision: Option<u64>,
    pub(super) credential_generation: Option<u64>,
    pub(super) desired: Vec<(String, Desired)>,
}

/// Everything the MCP supervisor owns between frames.
///
/// This is the single ECS resource the MCP layer declares: the server map, the
/// in-flight connects, the backoff bookkeeping, the event channel and the
/// settings snapshot all live here, so nothing else in the host can observe
/// them in an inconsistent combination.
#[derive(Resource, Default)]
pub(crate) struct McpSupervisor {
    pub(super) servers: McpServers,
    pub(super) connecting: Connecting,
    pub(super) failures: McpFailures,
    pub(super) events: EventChannel,
    pub(super) settings: McpSettings,
    /// The wake handle retry timers poke. Set once at startup.
    pub(super) wake: Option<Wake>,
}

impl McpSupervisor {
    /// The event sender a connect task reports on.
    pub(super) fn sender(&self) -> mpsc::Sender<Event> {
        self.events.sender.clone()
    }
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
    let mut supervisor = std::mem::take(&mut *world.resource_mut::<McpSupervisor>());
    if closing {
        handle_closing_servers(world, &mut supervisor);
        *world.resource_mut::<McpSupervisor>() = supervisor;
        return;
    }
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) else {
        *world.resource_mut::<McpSupervisor>() = supervisor;
        return;
    };
    let store = world.resource::<Store>().clone();
    let revision = match store.settings_repo().settings_revision() {
        Ok(revision) => revision,
        Err(error) => {
            if error.is_storage() {
                crate::host::fail(world, error.to_string());
            }
            *world.resource_mut::<McpSupervisor>() = supervisor;
            return;
        }
    };
    let credentials = world.get_resource::<Credentials>().cloned();
    match reload_desired_settings(
        &mut supervisor.settings,
        &store,
        credentials.as_ref(),
        revision,
    ) {
        ReloadOutcome::Ready => {}
        ReloadOutcome::Skip => {
            *world.resource_mut::<McpSupervisor>() = supervisor;
            return;
        }
        ReloadOutcome::Fail(message) => {
            *world.resource_mut::<McpSupervisor>() = supervisor;
            crate::host::fail(world, message);
            return;
        }
    }
    let wanted = supervisor.settings.desired.clone();
    prune_connecting(&mut supervisor.connecting, &wanted);
    prune_failures(&mut supervisor.failures, &wanted);
    let events = drain(&mut supervisor.events);
    for event in events {
        accept_event(world, &mut supervisor, &wanted, event, &runtime);
    }
    let retire = collect_retire_ids(&supervisor.servers, &wanted);
    for id in retire {
        retire_one(world, &mut supervisor, &id);
    }
    let targets = plan_connect_targets(
        &supervisor.servers,
        &supervisor.connecting,
        &supervisor.failures,
        wanted,
        Instant::now(),
    );
    if targets.is_empty() {
        project_servers(world, &supervisor);
        *world.resource_mut::<McpSupervisor>() = supervisor;
        return;
    }
    let sender = supervisor.sender();
    let wake = world.get_resource::<Wake>().cloned();
    let launched = spawn_connect_tasks(targets, &sender, wake, &runtime);
    supervisor.connecting.0.extend(launched);
    project_servers(world, &supervisor);
    *world.resource_mut::<McpSupervisor>() = supervisor;
}

fn handle_closing_servers(world: &mut World, supervisor: &mut McpSupervisor) {
    supervisor.connecting.0.clear();
    supervisor.failures.0.clear();
    let ids: Vec<String> = supervisor.servers.0.keys().cloned().collect();
    for id in ids {
        retire_one(world, supervisor, &id);
    }
    if let Some(runtime) = world.get_resource::<HostRuntime>().map(|it| it.0.clone()) {
        for event in drain(&mut supervisor.events) {
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
    supervisor: &mut McpSupervisor,
    wanted: &[(String, Desired)],
    event: Event,
    runtime: &tokio::runtime::Handle,
) {
    let (id, launch) = match &event {
        Event::Connected { id, launch, .. } | Event::Failed { id, launch, .. } => (id, launch),
    };
    let current = supervisor
        .connecting
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
    supervisor.connecting.0.remove(id);
    match event {
        Event::Connected {
            id,
            launch,
            connection,
            registry,
        } => {
            supervisor.failures.0.remove(&id);
            let Some((_, want)) = wanted.iter().find(|(wanted, _)| wanted == &id) else {
                return;
            };
            attach(
                world,
                supervisor,
                super::projection::Connected {
                    id,
                    launch,
                    connection,
                    registry,
                },
                want,
            );
        }
        Event::Failed {
            id,
            launch,
            reason,
            config_error,
        } => {
            tracing::warn!(connection = %id, config_error, "the MCP server did not connect: {reason}");
            let wake = supervisor.wake.clone();
            record_failure(
                &mut supervisor.failures,
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

/// Spreads a retry delay over the second half of its window.
///
/// Several MCP servers retry inside one host process on timers that start at
/// the same moment, so a host restart makes them all come back together.
/// Jitter is applied here, where the delay is scheduled, rather than inside
/// `backoff_delay`, which stays a pure function a test can assert exactly.
fn with_jitter(delay: std::time::Duration) -> std::time::Duration {
    let window = delay.as_millis() as u64 / 2;
    if window == 0 {
        return delay;
    }
    let mut bytes = [0u8; 8];
    // A failed entropy read falls back to no jitter: a slightly synchronised
    // retry is not worth failing a reconnect over.
    if getrandom::fill(&mut bytes).is_err() {
        return delay;
    }
    let offset = u64::from_le_bytes(bytes) % window;
    delay - std::time::Duration::from_millis(offset)
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
        let delay = with_jitter(backoff_delay(attempts));
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

fn drain(events: &mut EventChannel) -> Vec<Event> {
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
pub(crate) fn request_reconnect(supervisor: &mut McpSupervisor, id: &str) {
    supervisor.failures.0.remove(id);
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
    let supervisor = world.get_resource::<McpSupervisor>();
    check_state_from(
        &CheckContext {
            cred_gen: world
                .get_resource::<Credentials>()
                .map(Credentials::generation),
            servers: supervisor.map(|supervisor| &supervisor.servers),
            connecting: supervisor.map(|supervisor| &supervisor.connecting),
            failures: supervisor.map(|supervisor| &supervisor.failures),
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

#[cfg(test)]
mod jitter_tests {
    use super::with_jitter;
    use std::time::Duration;

    /// Jitter must never extend a retry, and must stay inside the second half
    /// of the window, so a delay of 32s lands somewhere in 16..=32s.
    #[test]
    fn jitter_stays_inside_the_lower_half_of_the_window() {
        let delay = Duration::from_secs(32);
        let floor = delay - Duration::from_secs(16);
        for _ in 0..200 {
            let jittered = with_jitter(delay);
            assert!(
                jittered <= delay,
                "jitter must not extend the delay: {jittered:?} > {delay:?}"
            );
            assert!(
                jittered >= floor,
                "jitter must stay in the lower half: {jittered:?} < {floor:?}"
            );
        }
    }

    /// A delay too short to have a half-millisecond window is returned as-is
    /// rather than divided down to zero.
    #[test]
    fn a_sub_millisecond_delay_is_unchanged() {
        let delay = Duration::from_micros(500);
        assert_eq!(with_jitter(delay), delay);
        assert_eq!(with_jitter(Duration::ZERO), Duration::ZERO);
    }
}
