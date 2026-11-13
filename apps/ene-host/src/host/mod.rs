//! The host: one headless Rig app on a thread of its own, the SQLite state and
//! the notification outbox.
//!
//! Every RPC operation is serialized onto the app thread, so SQLite, the ECS
//! world and the conversation log never race. Reads do not advance the agent
//! loop; mutations do, and the frame's notifications are flushed after it.
pub(crate) mod agent;
pub(crate) mod autonomy;
pub(crate) mod chat;
pub(crate) mod client;
pub(crate) mod compaction;
pub(crate) mod db;
pub(crate) mod error;
pub(crate) mod json;
pub(crate) mod mind;
pub(crate) mod model;
pub(crate) mod notify;
pub(crate) mod permission;
pub(crate) mod resources;
pub(crate) mod settings;
pub(crate) mod tools;

#[cfg(test)]
mod tests;
pub(crate) mod wire;
pub(crate) mod work;

use self::{
    db::Store,
    error::{HostError, Result},
    model::Credentials,
    notify::Notifier,
    permission::Approvals,
    resources::{DeviceHub, DeviceTimeout, HostState, Options, Perception, Waiting},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::{prelude::*, session_log::SessionLog, session_log::SessionStore};
use rig_memory::DirStore;
use std::{
    error::Error,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

const QUEUE: usize = 256;
const MAX_COMPANIONS: usize = 16;

type StartError = Box<dyn Error + Send + Sync>;
type Job = Box<dyn FnOnce(&mut App) + Send>;

enum Step {
    Job(Job),
    Wake,
    Stop,
}

/// The connection.json endpoint and the host's Owner Bearer stay the only
/// authentication; see docs/design/host-access.md.
#[derive(Clone)]
pub(crate) struct Runtime {
    handle: Arc<Handle>,
    credentials: Credentials,
    hub: DeviceHub,
}

struct Handle {
    commands: crossbeam_channel::Sender<Job>,
    shutdown: crossbeam_channel::Sender<()>,
    thread: Mutex<Option<JoinHandle<Result<()>>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        if let Ok(mut thread) = self.thread.lock()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

impl Runtime {
    pub(crate) async fn start(state: &Path) -> std::result::Result<Self, StartError> {
        Self::start_with(state, Options::default()).await
    }

    pub(crate) async fn start_with(
        state: &Path,
        options: Options,
    ) -> std::result::Result<Self, StartError> {
        let store = Store::open(&state.join("host.sqlite3")).map_err(boxed)?;
        let session_only = std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some();
        let credentials = Credentials::load(&store, session_only);
        let logs = DirStore::open(state.join("rig")).map_err(boxed)?;
        let (commands, command_rx) = crossbeam_channel::bounded::<Job>(QUEUE);
        let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let (shutdown, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let hub = DeviceHub::default();
        let device_timeout = options.device_timeout;
        let wake = wake_tx.clone();
        let thread_hub = hub.clone();
        let thread_store = store.clone();
        let thread_credentials = credentials.clone();
        let thread = std::thread::Builder::new()
            .name("ene-host-rig".into())
            .spawn(move || {
                let mut app = App::new();
                app.set_error_handler(bevy_ecs::error::warn)
                    .add_plugins(TaskPoolPlugin {
                        task_pool_options: TaskPoolOptions {
                            max_total_threads: 4,
                            ..Default::default()
                        },
                    })
                    .insert_resource(thread_store)
                    .insert_resource(thread_credentials)
                    .insert_resource(thread_hub.clone())
                    .insert_resource(HostState::default())
                    .insert_resource(Notifier::new())
                    .insert_resource(Waiting::default())
                    .insert_resource(Perception::default())
                    .insert_resource(Approvals::default())
                    .insert_resource(DeviceTimeout(device_timeout))
                    .insert_resource(rig_ecs::tasks::Wake::new(move || {
                        let _ = wake.try_send(());
                    }))
                    .insert_resource(SessionStore::new(logs))
                    .add_plugins((
                        AgentPlugin,
                        SessionLogPlugin,
                        agent::AgentPlugin,
                        permission::GatePlugin,
                        client::ClientPlugin,
                        work::WorkPlugin,
                        mind::MindPlugin,
                        autonomy::AutonomyPlugin,
                        compaction::CompactionPlugin,
                    ));
                app.finish();
                if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
                    time.set_max_delta(Duration::from_secs(60));
                }
                let result = (|| -> Result<()> {
                    restore(app.world_mut())?;
                    tick(&mut app);
                    restore_after_first_frame(app.world_mut())?;
                    Ok(())
                })();
                let ok = result.is_ok();
                let _ = ready_tx.send(result);
                if !ok {
                    return Ok(());
                }
                loop {
                    match next(&command_rx, &wake_rx, &shutdown_rx) {
                        Step::Job(job) => job(&mut app),
                        Step::Wake => tick(&mut app),
                        Step::Stop => {
                            app.world_mut().resource_mut::<HostState>().closing = true;
                            app.world_mut().write_message(AppExit::Success);
                            tick(&mut app);
                            return match &app.world().resource::<HostState>().failed {
                                Some(reason) => Err(HostError::unavailable(reason.clone())),
                                None => Ok(()),
                            };
                        }
                    }
                }
            })
            .map_err(boxed)?;
        ready_rx
            .recv()
            .map_err(|_| boxed("Rig kernel did not start"))?
            .map_err(boxed)?;
        Ok(Self {
            handle: Arc::new(Handle {
                commands,
                shutdown,
                thread: Mutex::new(Some(thread)),
            }),
            credentials,
            hub,
        })
    }

    pub(crate) fn credentials(&self) -> Credentials {
        self.credentials.clone()
    }

    pub(crate) fn hub(&self) -> DeviceHub {
        self.hub.clone()
    }

    /// Runs `operation` on the app thread, advances one frame and flushes the
    /// notifications it produced.
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.send(move |reply| {
            Box::new(move |app| {
                let result = app
                    .world()
                    .resource::<HostState>()
                    .ready()
                    .and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), error.to_string());
                }
                tick(app);
                let result = app.world().resource::<HostState>().ready().and(result);
                let _ = reply.send(result);
            })
        })
        .await
    }

    /// Runs `operation` without advancing the agent loop.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.send(move |reply| {
            Box::new(move |app| {
                let result = app
                    .world()
                    .resource::<HostState>()
                    .ready()
                    .and_then(|()| operation(app));
                if let Err(error) = &result
                    && error.is_storage()
                {
                    fail(app.world_mut(), error.to_string());
                }
                let _ = reply.send(result);
            })
        })
        .await
    }

    async fn send<T: Send + 'static>(
        &self,
        job: impl FnOnce(oneshot::Sender<Result<T>>) -> Job,
    ) -> Result<T> {
        let (reply, received) = oneshot::channel();
        self.handle
            .commands
            .try_send(job(reply))
            .map_err(|_| HostError::exhausted("host command queue is full or closed"))?;
        received
            .await
            .map_err(|_| HostError::unavailable("Rig kernel stopped"))?
    }

    pub(crate) async fn begin_shutdown(&self) {
        let _ = self
            .call(|app| {
                app.world_mut().resource_mut::<HostState>().closing = true;
                Ok(())
            })
            .await;
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let shutdown = self.handle.shutdown.clone();
        let thread = self
            .handle
            .thread
            .lock()
            .map_err(|_| HostError::internal("Rig kernel lock failed"))?
            .take();
        tokio::task::spawn_blocking(move || {
            thread.map_or(Ok(()), |thread| {
                let _ = shutdown.try_send(());
                thread
                    .join()
                    .map_err(|_| HostError::internal("Rig kernel panicked"))?
            })
        })
        .await
        .map_err(|_| HostError::internal("Rig kernel join failed"))?
    }
}

fn next(
    commands: &crossbeam_channel::Receiver<Job>,
    wake: &crossbeam_channel::Receiver<()>,
    shutdown: &crossbeam_channel::Receiver<()>,
) -> Step {
    // Cancellation requests complete on worker threads; a frame every second
    // keeps deadlines and timeouts moving while nobody calls the API.
    crossbeam_channel::select_biased! {
        recv(shutdown) -> _ => Step::Stop,
        recv(commands) -> job => match job {
            Ok(job) => Step::Job(job),
            Err(_) => Step::Stop,
        },
        recv(wake) -> _ => Step::Wake,
        default(Duration::from_secs(1)) => Step::Wake,
    }
}

/// One frame: run the app, notice a lost session log, and flush notifications.
pub(crate) fn tick(app: &mut App) {
    app.update();
    if let Some(reason) = app.world().resource::<SessionLog>().failure() {
        fail(app.world_mut(), reason);
    }
    if app.world().resource::<HostState>().failed.is_some() {
        app.world_mut().resource_mut::<Notifier>().clear();
        return;
    }
    app.world_mut().resource_mut::<Notifier>().flush();
}

/// A sticky host failure: it stops the agent loop and refuses new work.
pub(crate) fn fail(world: &mut World, reason: String) {
    if world.resource::<HostState>().failed.is_some() {
        return;
    }
    tracing::error!(%reason, "the host stopped");
    let agents: Vec<Entity> = world
        .query_filtered::<Entity, With<Agent>>()
        .iter(world)
        .collect();
    for agent in agents {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some("host failure".into()),
        });
    }
    world.resource_mut::<HostState>().failed = Some(reason.clone());
    world.resource_mut::<Notifier>().fail(&reason);
}

/// Restores state that must not wait for the ECS frame: stale approvals and
/// operations whose answers can no longer arrive.
fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store.expire_pending_approvals()?;
    store.mark_unanswered_operations_uncertain()?;
    Ok(())
}

/// Restores what needs a restorable session: the agents and their models.
fn restore_after_first_frame(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    for companion in store.companions()? {
        agent::sync_companion(world, &companion.id)?;
    }
    Ok(())
}

fn boxed(error: impl std::fmt::Display) -> StartError {
    std::io::Error::other(error.to_string()).into()
}

/// The whole public state, used for `GetSnapshot` and `Subscribe`.
pub(crate) fn snapshot(app: &mut App) -> Result<w::Snapshot> {
    let store = app.world().resource::<Store>().clone();
    let state = app.world().resource::<HostState>();
    let notifier = app.world().resource::<Notifier>();
    let hub = app.world().resource::<DeviceHub>();
    let companions: Vec<w::Companion> = store.companions()?.iter().map(wire::companion).collect();
    let activations = store.activations()?;
    let active_companion_id = activations
        .iter()
        .find(|activation| activation.client_id.is_some())
        .map(|activation| activation.companion_id.clone());
    let work_sessions: Vec<w::WorkSession> = companions
        .iter()
        .filter_map(|companion| store.work_sessions(&companion.id).ok())
        .flatten()
        .map(|session| wire::work_session(&session))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let work_sessions = work_sessions
        .into_iter()
        .filter(|session| seen.insert(session.id.clone()))
        .collect();
    let clients = client::clients(&store, hub)?;
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
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            adapters: settings::providers()
                .into_iter()
                .map(|provider| provider.adapter)
                .collect(),
            transport: "/rpc".into(),
            ..Default::default()
        }
        .into(),
        settings: Some(wire::settings(&store.settings()?)).into(),
        companions,
        work_sessions,
        active_companion_id,
        pending_approvals: store
            .approvals(permission::ApprovalStatus::Pending)?
            .iter()
            .map(wire::approval)
            .collect(),
        clients,
        cursor: Some(notifier.cursor()).into(),
        policy_rules: store
            .policy_rules()?
            .iter()
            .map(wire::policy_rule)
            .collect(),
        activations: activations.iter().map(wire::activation).collect(),
        ..Default::default()
    })
}

pub(crate) fn subscribe(
    app: &mut App,
    client_id: Option<&str>,
) -> Result<(w::Snapshot, broadcast::Receiver<w::Notification>)> {
    let store = app.world().resource::<Store>().clone();
    if let Some(client_id) = client_id.filter(|id| !id.is_empty()) {
        db::validate_id(client_id)?;
        if store.client(client_id)?.is_none() {
            return Err(HostError::not_found("client not found"));
        }
    }
    let receiver = app.world().resource::<Notifier>().subscribe();
    Ok((snapshot(app)?, receiver))
}
