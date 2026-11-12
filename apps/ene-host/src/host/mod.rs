//! One headless Rig app, its command thread, resources and shutdown.
pub(crate) mod agent;
pub(crate) mod body;
pub(crate) mod chat;
pub(crate) mod client;
pub(crate) mod compaction;
pub(crate) mod companion;
pub(crate) mod consolidation;
pub(crate) mod db;
pub(crate) mod error;
pub(crate) mod failure;
pub(crate) mod memory;
pub(crate) mod mind;
pub(crate) mod model;
pub(crate) mod notify;
pub(crate) mod resources;
pub(crate) mod settings;
pub(crate) mod tools;
pub(crate) mod wire;
pub(crate) mod work;

#[cfg(test)]
mod tests;

use self::{
    agent::AgentKey,
    db::{Result, Store},
    error::HostError,
    model::{Binding, Credentials, ModelSource, Providers},
    notify::Notifier,
    resources::{
        CompactionQueue, ConsolidationQueue, DeviceStreams, DeviceTimeout, HostOptions, HostState,
        Inactivity, Models,
    },
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::{prelude::*, session_log::SessionLog};
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

#[derive(Clone)]
pub(crate) struct Runtime {
    handle: Arc<Handle>,
    credentials: Credentials,
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

pub(crate) struct Options {
    pub(crate) models: Option<Arc<dyn ModelSource>>,
    pub(crate) consolidate: bool,
    pub(crate) device_timeout: Duration,
    pub(crate) compaction: compaction::Options,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            models: None,
            consolidate: true,
            device_timeout: Duration::from_secs(120),
            compaction: compaction::Options::default(),
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
        let credentials = Credentials::load(&store).await;
        let logs = DirStore::open(state.join("rig")).map_err(boxed)?;
        let (commands, command_rx) = crossbeam_channel::bounded::<Job>(QUEUE);
        let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let (shutdown, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let models: Arc<dyn ModelSource> = options.models.clone().unwrap_or_else(|| {
            Arc::new(Providers {
                credentials: credentials.clone(),
            })
        });
        let host_options = HostOptions {
            consolidate: options.consolidate,
            compaction: options.compaction.clone(),
        };
        let wake = wake_tx.clone();
        let thread = std::thread::Builder::new()
            .name("ene-host-rig".into())
            .spawn(move || {
                let mut app = App::new();
                app.add_plugins(TaskPoolPlugin {
                    task_pool_options: TaskPoolOptions {
                        max_total_threads: 3,
                        ..Default::default()
                    },
                })
                .insert_resource(Wake::new(move || {
                    let _ = wake.try_send(());
                }))
                .insert_resource(store)
                .insert_resource(HostState::default())
                .insert_resource(Notifier::new())
                .insert_resource(Models(models))
                .insert_resource(DeviceStreams::default())
                .insert_resource(DeviceTimeout(options.device_timeout))
                .insert_resource(ConsolidationQueue::default())
                .insert_resource(CompactionQueue::default())
                .insert_resource(Inactivity::default())
                .insert_resource(resources::Perception::default())
                .insert_resource(host_options)
                .add_plugins((AgentPlugin, SessionLogPlugin, agent::KernelPlugin));
                app.finish();
                if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
                    time.set_max_delta(Duration::from_secs(60));
                }
                let result = rig_ecs::session_log::open_session(app.world_mut(), Arc::new(logs))
                    .map_err(|error| HostError::storage(error.to_string()))
                    .and_then(|()| {
                        tick(&mut app);
                        restore(&mut app)
                    });
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
                            revoke_and_stop(app.world_mut(), None);
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
        })
    }

    pub(crate) fn credentials(&self) -> Credentials {
        self.credentials.clone()
    }

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
                if result.as_ref().is_err_and(HostError::is_storage) {
                    fail(app.world_mut(), "state storage failed".into());
                }
                tick(app);
                let result = app.world().resource::<HostState>().ready().and(result);
                let _ = reply.send(result);
            })
        })
        .await
    }

    /// Runs a read without advancing the agent loop or virtual time.
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
                if result.as_ref().is_err_and(HostError::is_storage) {
                    fail(app.world_mut(), "state storage failed".into());
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

pub(crate) fn fail(world: &mut World, reason: String) {
    if world.resource::<HostState>().failed.is_some() {
        return;
    }
    tracing::error!(%reason, "the host stopped");
    revoke_and_stop(world, None);
    world.resource_mut::<HostState>().failed = Some(reason.clone());
    world.resource_mut::<Notifier>().fail(&reason);
}

pub(crate) fn revoke_and_stop(world: &mut World, companion: Option<&str>) {
    let agents: Vec<(Entity, AgentKey)> = world
        .query::<(Entity, &AgentKey)>()
        .iter(world)
        .filter(|(_, key)| companion.is_none_or(|id| key.companion() == id))
        .map(|(entity, key)| (entity, key.clone()))
        .collect();
    for (entity, _) in &agents {
        if let Some(binding) = world.get::<Binding>(*entity) {
            model::revoke(binding);
        }
        world
            .entity_mut(*entity)
            .remove::<(Binding, rig_ecs::model::Connection)>();
        world.trigger(StopTurn {
            entity: *entity,
            reason: Some("permissions changed".into()),
        });
    }
    let calls: Vec<(Entity, Binding)> = world
        .query::<(Entity, &ForAgent, &Binding)>()
        .iter(world)
        .filter(|(_, agent, _)| agents.iter().any(|(entity, _)| *entity == agent.0))
        .map(|(call, _, binding)| (call, binding.clone()))
        .collect();
    for (call, binding) in calls {
        model::revoke(&binding);
        world.entity_mut(call).remove::<Binding>();
    }
}

fn restore(app: &mut App) -> Result<()> {
    app.world().resource::<Store>().expire_pending_approvals()?;
    agent::restore(app.world_mut())?;
    compaction::restore(app.world_mut());
    work::restore(app.world_mut())?;
    client::restore(app.world_mut())?;
    mind::restore(app.world_mut())?;
    tick(app);
    Ok(())
}

pub(crate) fn snapshot(app: &mut App) -> Result<w::Snapshot> {
    let store = app.world().resource::<Store>().clone();
    let state = app.world().resource::<HostState>();
    let notifier = app.world().resource::<Notifier>();
    let streams = app.world().resource::<DeviceStreams>();
    let companions = mind::companions(&store)?;
    let clients = client::list(&store, streams)?;
    let settings = settings::settings(&store)?;
    let works = store.works()?.iter().map(wire::work_session).collect();
    let active_companion_id = store
        .activations()?
        .into_iter()
        .next()
        .map(|(companion, _, _)| companion);
    let health = w::HostHealth {
        ready: state.ready().is_ok(),
        recovery_required: state.failed.is_some(),
        stopping: state.closing,
        ..Default::default()
    };
    Ok(w::Snapshot {
        server_version: env!("CARGO_PKG_VERSION").into(),
        health: health.into(),
        capabilities: w::HostCapabilities {
            limits: wire::json(&serde_json::json!({
                "max_input_tokens": chat::MAX_INPUT_TOKENS,
                "max_input_chars": chat::MAX_INPUT_CHARS,
                "max_companions": MAX_COMPANIONS,
            })),
            features: [
                "history",
                "notifications",
                "chat",
                "connections",
                "credentials",
                "mind",
                "work",
                "clients",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            adapters: model::descriptors()
                .into_iter()
                .map(|provider| provider.id)
                .collect(),
            transport: "/rpc".into(),
            ..Default::default()
        }
        .into(),
        settings: settings.into(),
        companions,
        work_sessions: works,
        active_companion_id,
        pending_approvals: store.pending_approvals()?,
        clients,
        cursor: Some(notifier.cursor()).into(),
        policy_rules: store.policy_rules()?,
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

fn boxed(error: impl std::fmt::Display) -> StartError {
    std::io::Error::other(error.to_string()).into()
}
