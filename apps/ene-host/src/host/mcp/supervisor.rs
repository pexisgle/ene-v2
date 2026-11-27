//! Reconciles the desired MCP settings with one live phase per connection.
use super::{
    projection::{project_servers, snapshot_tools},
    registration::{McpRegistration, Receiver},
    settings::{ConnectionStamp, DesiredServers, McpSettings, reload_desired_settings},
    startup,
};
use crate::host::{db::Store, model::Credentials, resources::HostRuntime};
use bevy_ecs::prelude::*;
use ene_work::mcp::{ConnectFailure, backoff_delay};
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use rig_ecs::prelude::*;
use rig_rmcp::McpConnection;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::{
    sync::{CancellationToken, DropGuard},
    task::AbortOnDropHandle,
};

/// Bound completed handshakes waiting for adoption by the host actor.
const QUEUE: usize = 32;
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Launch {
    pub(super) stamp: ConnectionStamp,
    pub(super) owner: String,
}

/// Live state returned to connection probes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckState {
    Connected(Vec<String>),
    Failed(String),
    Connecting,
}

pub(super) struct Slot {
    pub(super) launch: Launch,
    pub(super) attempts: u32,
    pub(super) stop: DropGuard,
    pub(super) phase: Phase,
}

pub(super) enum Phase {
    Opening {
        _task: AbortOnDropHandle<()>,
    },
    Running(Server),
    Failed {
        failure: ConnectFailure,
        retry: Option<Retry>,
    },
}

pub(super) struct Retry {
    at: Instant,
    _timer: Option<AbortOnDropHandle<()>>,
}

impl Slot {
    fn fail(
        &mut self,
        failure: ConnectFailure,
        wake: Option<&Wake>,
        runtime: &tokio::runtime::Handle,
    ) {
        let retry = if matches!(failure, ConnectFailure::Configuration(_)) {
            self.attempts = 0;
            None
        } else {
            self.attempts = self.attempts.saturating_add(1);
            let at = Instant::now() + backoff_delay(self.attempts);
            let timer = wake.cloned().map(|wake| {
                AbortOnDropHandle::new(runtime.spawn(async move {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                    wake.wake();
                }))
            });
            Some(Retry { at, _timer: timer })
        };
        self.phase = Phase::Failed { failure, retry };
    }
}

pub(super) struct Server {
    pub(super) connection: McpConnection,
    pub(super) receiver: Receiver,
    pub(super) registry: ToolServerHandle,
    pub(super) connected_at: Instant,
    pub(super) label: String,
    pub(super) allowed: Vec<String>,
}

pub(super) enum Event {
    Connected {
        id: String,
        launch: Launch,
        connection: McpConnection,
        receiver: Receiver,
        registry: ToolServerHandle,
    },
    Failed {
        id: String,
        launch: Launch,
        failure: ConnectFailure,
    },
}

impl Event {
    fn retire(self, runtime: &tokio::runtime::Handle) {
        if let Self::Connected { connection, .. } = self {
            close_connection(connection, runtime);
        }
    }
}

struct EventChannel {
    sender: mpsc::Sender<Event>,
    receiver: mpsc::Receiver<Event>,
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
    pub(super) fn lock(&self) -> parking_lot::MutexGuard<'_, McpSupervisor> {
        self.0.lock()
    }

    pub(crate) fn request_reconnect(&self, id: &str, stamp: ConnectionStamp) {
        let mut supervisor = self.lock();
        let Some(slot) = supervisor
            .slots
            .get_mut(id)
            .filter(|slot| slot.launch.stamp == stamp)
        else {
            return;
        };
        match &slot.phase {
            Phase::Failed { .. } => {
                supervisor.slots.remove(id);
            }
            Phase::Running(server) if server.connection.is_closed() => {
                supervisor.slots.remove(id);
            }
            _ => slot.attempts = 0,
        }
    }

    pub(crate) fn check_state(&self, id: &str, stamp: ConnectionStamp) -> CheckState {
        let supervisor = self.lock();
        match supervisor
            .slots
            .get(id)
            .filter(|slot| slot.launch.stamp == stamp)
            .map(|slot| &slot.phase)
        {
            Some(Phase::Running(server)) if !server.connection.is_closed() => {
                let mut names: Vec<_> = server
                    .registry
                    .snapshot()
                    .names()
                    .map(str::to_owned)
                    .collect();
                names.sort_unstable();
                CheckState::Connected(names)
            }
            Some(Phase::Failed { failure, .. }) => CheckState::Failed(failure.reason().to_owned()),
            _ => CheckState::Connecting,
        }
    }

    pub(crate) fn tool_is_current(&self, tool: &McpRegistration) -> bool {
        self.lock().slots.get(&tool.server).is_some_and(|slot| {
            slot.launch == tool.launch && !slot.stop.token().is_cancelled()
                && matches!(&slot.phase, Phase::Running(server)
                    if !server.connection.is_closed() && server.registry.snapshot()
                        .get(tool.definition.name.as_str())
                        .is_some_and(|registered| registered.key() == &tool.key && registered.is_live()))
        })
    }
}

#[derive(Default)]
pub(super) struct McpSupervisor {
    pub(super) slots: HashMap<String, Slot>,
    events: EventChannel,
    settings: McpSettings,
}

/// Reconcile transports first, then publish their tool registries to Rig.
pub(super) fn sync(world: &mut World) {
    let projection: Option<crate::host::db::Result<_>> = {
        let mut supervisor = world.resource::<Mcp>().lock();
        let runtime = world
            .get_resource::<HostRuntime>()
            .map(|runtime| &runtime.0);
        if world
            .get_resource::<crate::host::resources::HostState>()
            .is_some_and(|state| state.ready().is_err())
        {
            supervisor.close(runtime);
            Some(Ok(HashMap::new()))
        } else {
            runtime.map(|runtime| {
                supervisor.reconcile(
                    world.resource::<Store>(),
                    world.get_resource::<Credentials>(),
                    world.get_resource::<Wake>(),
                    runtime,
                )?;
                Ok(snapshot_tools(&supervisor.slots))
            })
        }
    };
    match projection {
        Some(Ok(tools)) => {
            project_servers(world, tools);
        }
        Some(Err(error)) if error.is_storage() => crate::host::fail(world, &error.to_string()),
        _ => {}
    }
}

impl McpSupervisor {
    fn reconcile(
        &mut self,
        store: &Store,
        credentials: Option<&Credentials>,
        wake: Option<&Wake>,
        runtime: &tokio::runtime::Handle,
    ) -> crate::host::db::Result<()> {
        reload_desired_settings(&mut self.settings, store, credentials)?;
        let wanted = self.settings.desired.clone();
        while let Ok(event) = self.events.receiver.try_recv() {
            self.accept_event(&wanted, event, wake, runtime);
        }
        self.slots.retain(|id, slot| {
            let current = wanted
                .get(id)
                .is_some_and(|want| want.stamp == slot.launch.stamp);
            if let Phase::Running(server) = &slot.phase
                && (!current || server.connection.is_closed())
            {
                slot.stop.token().cancel();
                close_connection(server.connection.clone(), runtime);
                if current {
                    if server.connected_at.elapsed() >= STABLE_CONNECTION {
                        slot.attempts = 0;
                    }
                    slot.fail(
                        ConnectFailure::Transient("the MCP server disconnected".into()),
                        wake,
                        runtime,
                    );
                }
            }
            current
        });
        self.start_connections(&wanted, store, credentials, wake, runtime);
        Ok(())
    }

    fn start_connections(
        &mut self,
        wanted: &DesiredServers,
        store: &Store,
        credentials: Option<&Credentials>,
        wake: Option<&Wake>,
        runtime: &tokio::runtime::Handle,
    ) {
        let now = Instant::now();
        for (id, want) in wanted {
            let attempts = match self.slots.get(id) {
                Some(Slot {
                    phase:
                        Phase::Failed {
                            retry: Some(retry), ..
                        },
                    attempts,
                    ..
                }) if now >= retry.at => *attempts,
                Some(_) => continue,
                None => 0,
            };
            let registry = ToolServer::new().run();
            let launch = Launch {
                stamp: want.stamp,
                owner: registry.owner(),
            };
            let event_id = id.clone();
            let event_launch = launch.clone();
            let want = want.clone();
            let sender = self.events.sender.clone();
            let credentials = credentials.cloned();
            let store = store.clone();
            let wake = wake.cloned();
            let stop = CancellationToken::new();
            let lifetime = stop.clone().drop_guard();
            let task = runtime.spawn(Box::pin(async move {
                let opened = startup::start(
                    &store,
                    &event_id,
                    &want,
                    credentials.as_ref(),
                    registry.clone(),
                    &stop,
                )
                .await;
                let event = match opened {
                    Ok((connection, receiver)) => Event::Connected {
                        id: event_id,
                        launch: event_launch,
                        connection,
                        receiver,
                        registry,
                    },
                    Err(failure) => Event::Failed {
                        id: event_id,
                        launch: event_launch,
                        failure,
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
            self.slots.insert(
                id.clone(),
                Slot {
                    launch,
                    attempts,
                    stop: lifetime,
                    phase: Phase::Opening {
                        _task: AbortOnDropHandle::new(task),
                    },
                },
            );
        }
    }

    pub(super) fn accept_event(
        &mut self,
        wanted: &DesiredServers,
        event: Event,
        wake: Option<&Wake>,
        runtime: &tokio::runtime::Handle,
    ) {
        let (id, launch) = match &event {
            Event::Connected { id, launch, .. } | Event::Failed { id, launch, .. } => (id, launch),
        };
        let Some((slot, want)) =
            self.slots
                .get_mut(id)
                .zip(wanted.get(id))
                .filter(|(slot, want)| {
                    slot.launch == *launch
                        && want.stamp == launch.stamp
                        && matches!(slot.phase, Phase::Opening { .. })
                })
        else {
            event.retire(runtime);
            return;
        };
        match event {
            Event::Connected {
                connection,
                receiver,
                registry,
                ..
            } => {
                slot.phase = Phase::Running(Server {
                    connection,
                    receiver,
                    registry,
                    connected_at: Instant::now(),
                    label: want.name.clone(),
                    allowed: want.config.allowed_tools.clone(),
                });
            }
            Event::Failed { id, failure, .. } => {
                tracing::warn!(connection = %id, reason = failure.reason(), "the MCP server did not connect");
                slot.fail(failure, wake, runtime);
            }
        }
    }

    fn close(&mut self, runtime: Option<&tokio::runtime::Handle>) {
        for (_, slot) in self.slots.drain() {
            slot.stop.token().cancel();
            if let (Some(runtime), Phase::Running(server)) = (runtime, slot.phase) {
                close_connection(server.connection, runtime);
            }
        }
        while let Ok(event) = self.events.receiver.try_recv() {
            if let Some(runtime) = runtime {
                event.retire(runtime);
            }
        }
    }
}

pub(super) fn close_connection(connection: McpConnection, runtime: &tokio::runtime::Handle) {
    runtime.spawn(async move { connection.close().await });
}
