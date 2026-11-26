//! MCP connection supervisor: settings/credential diffing, backoff, auth
//! injection, stdio environment/cwd application, and background connection
//! lifecycle management.
use super::{
    failures::{FailureKind, FailureReport, McpFailures, prune_failures, record_failure},
    projection::{attach, project_servers, retire_one},
    settings::{
        McpSettings, ReloadOutcome, collect_retire_ids, prune_connecting, reload_desired_settings,
    },
};
use crate::host::{db::Store, model::Credentials, resources::HostRuntime};
use bevy_ecs::prelude::*;
pub(super) use ene_work::mcp::{CheckState, ConnectFailure, Desired, Launch};
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

/// Owns the upstream connection state shared by the actor and connection probes.
#[derive(Clone, Default, Resource)]
pub(crate) struct Mcp(std::sync::Arc<parking_lot::Mutex<McpSupervisor>>);

impl Mcp {
    pub(crate) fn lock(&self) -> parking_lot::MutexGuard<'_, McpSupervisor> {
        self.0.lock()
    }

    pub(crate) fn request_reconnect(&self, id: &str) {
        request_reconnect(&mut self.lock(), id);
    }

    pub(crate) fn check_state(
        &self,
        credentials: Option<&Credentials>,
        id: &str,
        revision: u64,
    ) -> CheckState {
        check_state(&self.lock(), credentials, id, revision)
    }
}

/// Everything the MCP supervisor owns between frames.
///
/// The server map, the
/// in-flight connects, the backoff bookkeeping, the event channel and the
/// settings snapshot all live here, so nothing else in the host can observe
/// them in an inconsistent combination.
#[derive(Default)]
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

/// Connects what the settings ask for, retires what they no longer do, and
/// attaches the tools a handler listed.
pub(super) fn sync(world: &mut World) {
    let closing = world
        .get_resource::<crate::host::resources::HostState>()
        .is_some_and(|state| state.ready().is_err());
    let mcp = world.resource::<Mcp>().clone();
    let mut supervisor = mcp.lock();
    if closing {
        handle_closing_servers(world, &mut supervisor);
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
                crate::host::fail(world, &error.to_string());
            }
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
            return;
        }
        ReloadOutcome::Fail(message) => {
            crate::host::fail(world, &message);
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
        return;
    }
    let sender = supervisor.sender();
    let wake = world.get_resource::<Wake>();
    let launched = spawn_connect_tasks(targets, &sender, wake, &runtime);
    supervisor.connecting.0.extend(launched);
    project_servers(world, &supervisor);
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
    wake: Option<&Wake>,
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
            let wake = wake.cloned();
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
pub(super) fn request_reconnect(supervisor: &mut McpSupervisor, id: &str) {
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
fn check_state(
    supervisor: &McpSupervisor,
    credentials: Option<&Credentials>,
    id: &str,
    revision: u64,
) -> CheckState {
    check_state_from(
        &CheckContext {
            cred_gen: credentials.map(Credentials::generation),
            servers: Some(&supervisor.servers),
            connecting: Some(&supervisor.connecting),
            failures: Some(&supervisor.failures),
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
