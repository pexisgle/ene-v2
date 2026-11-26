//! MCP connection supervisor: settings/credential diffing, backoff, auth
//! injection, stdio environment/cwd application, and background connection
//! lifecycle management.
use super::{
    failures::{FailureKind, FailureReport, McpFailures, prune_failures, record_failure},
    projection::project_servers,
    settings::{McpSettings, collect_retire_ids, prune_connecting, reload_desired_settings},
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
        check_state(
            &self.lock(),
            id,
            revision,
            credentials.map(Credentials::generation),
        )
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

/// Reconcile transports first, then publish their tool registries to Rig.
pub(super) fn sync(world: &mut World) {
    let mcp = world.resource::<Mcp>().clone();
    let mut supervisor = mcp.lock();
    let runtime = world
        .get_resource::<HostRuntime>()
        .map(|runtime| &runtime.0);
    if world
        .get_resource::<crate::host::resources::HostState>()
        .is_some_and(|state| state.ready().is_err())
    {
        supervisor.close(runtime);
        project_servers(world, &supervisor.servers, &[]);
    } else if let Some(runtime) = runtime {
        match supervisor.reconcile(
            world.resource::<Store>(),
            world.get_resource::<Credentials>(),
            world.get_resource::<Wake>(),
            runtime,
        ) {
            Ok(retired) => project_servers(world, &supervisor.servers, &retired),
            Err(error) if error.is_storage() => crate::host::fail(world, &error.to_string()),
            Err(_) => {}
        }
    }
}

impl McpSupervisor {
    fn reconcile(
        &mut self,
        store: &Store,
        credentials: Option<&Credentials>,
        wake: Option<&Wake>,
        runtime: &tokio::runtime::Handle,
    ) -> crate::host::db::Result<Vec<String>> {
        reload_desired_settings(&mut self.settings, store, credentials)?;
        let wanted = self.settings.desired.clone();
        prune_connecting(&mut self.connecting, &wanted);
        prune_failures(&mut self.failures, &wanted);
        let mut retired = Vec::new();
        while let Ok(event) = self.events.receiver.try_recv() {
            retired.extend(accept_event(self, &wanted, event, runtime));
        }
        for id in collect_retire_ids(&self.servers, &wanted) {
            if let Some(server) = self.servers.0.remove(&id) {
                close_connection(server.connection, runtime);
            }
        }
        let targets = plan_connect_targets(
            &self.servers,
            &self.connecting,
            &self.failures,
            &wanted,
            Instant::now(),
        );
        self.connecting.0.extend(spawn_connect_tasks(
            targets,
            &self.events.sender,
            wake,
            runtime,
        ));
        Ok(retired)
    }

    fn close(&mut self, runtime: Option<&tokio::runtime::Handle>) {
        self.connecting.0.clear();
        self.failures.0.clear();
        for (_, server) in self.servers.0.drain() {
            if let Some(runtime) = runtime {
                close_connection(server.connection, runtime);
            }
        }
        if let Some(runtime) = runtime {
            while let Ok(event) = self.events.receiver.try_recv() {
                if let Event::Connected { connection, .. } = event {
                    close_connection(connection, runtime);
                }
            }
        }
    }
}

pub(super) fn close_connection(connection: McpConnection, runtime: &tokio::runtime::Handle) {
    runtime.spawn(async move { connection.close().await });
}

fn plan_connect_targets(
    servers: &McpServers,
    connecting: &Connecting,
    failures: &McpFailures,
    wanted: &[(String, Desired)],
    now: Instant,
) -> Vec<(String, Desired)> {
    let mut seen = HashSet::new();
    wanted
        .iter()
        .filter(|(id, _)| {
            if !seen.insert(id) || servers.0.contains_key(id) || connecting.0.contains_key(id) {
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
        .cloned()
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
    supervisor: &mut McpSupervisor,
    wanted: &[(String, Desired)],
    event: Event,
    runtime: &tokio::runtime::Handle,
) -> Option<String> {
    let (id, launch) = match &event {
        Event::Connected { id, launch, .. } | Event::Failed { id, launch, .. } => (id, launch),
    };
    let want = wanted.iter().find(|(wanted, want)| {
        wanted == id
            && want.revision == launch.revision
            && want.credential_generation == launch.credential_generation
    });
    let current = supervisor
        .connecting
        .0
        .get(id)
        .is_some_and(|pending| &pending.launch == launch);
    let Some((_, want)) = want.filter(|_| current) else {
        if let Event::Connected { connection, .. } = event {
            close_connection(connection, runtime);
        }
        return None;
    };
    supervisor.connecting.0.remove(id);
    match event {
        Event::Connected {
            id,
            launch,
            connection,
            registry,
        } => {
            supervisor.failures.0.remove(&id);
            let previous = supervisor.servers.0.insert(
                id.clone(),
                Server {
                    connection,
                    launch,
                    registry,
                    label: want.name.clone(),
                    allowed: want.allowed.clone(),
                },
            )?;
            close_connection(previous.connection, runtime);
            Some(id)
        }
        Event::Failed {
            id,
            launch,
            reason,
            config_error,
        } => {
            tracing::warn!(connection = %id, config_error, "the MCP server did not connect: {reason}");
            record_failure(
                &mut supervisor.failures,
                FailureReport {
                    id,
                    launch,
                    reason,
                    config_error,
                },
                supervisor.wake.clone(),
                runtime,
            );
            None
        }
    }
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

/// Inspects the current MCP state for a specific configuration and credential generation.
fn check_state(
    supervisor: &McpSupervisor,
    id: &str,
    revision: u64,
    credential_generation: Option<u64>,
) -> CheckState {
    if let Some(server) = supervisor.servers.0.get(id)
        && server.launch.revision == revision
        && server.launch.credential_generation == credential_generation
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
    if let Some(failure) = supervisor.failures.0.get(id)
        && failure.revision == revision
        && failure.credential_generation == credential_generation
        && !supervisor
            .connecting
            .0
            .get(id)
            .is_some_and(|pending| pending.launch.revision == revision)
    {
        return CheckState::Failed(failure.reason.clone());
    }
    CheckState::Connecting
}
