//! Owns the headless Rig App, its command thread, host snapshots and shutdown.
pub(crate) mod chat;
pub(crate) mod client;
pub(crate) mod db;
pub(crate) mod kernel;
pub(crate) mod mind;
pub(crate) mod model;
pub(crate) mod settings;
pub(crate) mod work;

#[cfg(test)]
mod tests;

use self::{
    db::{Database, Result, document, is_storage, new_id, now, validate_id},
    mind::CompanionRecord,
    model::Credentials,
    work::WorkRecord,
};
use bevy_ecs::prelude::*;
use connectrpc::ConnectError;
use crossbeam_channel::{Receiver, Sender, bounded, select_biased};
use ene_rpc as w;
use rig_ecs::{
    prelude::*,
    session_log::{SessionLog, open_session},
};
use rig_memory::DirStore;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

type StartError = Box<dyn std::error::Error + Send + Sync>;

const EVENTS: usize = 256;
const QUEUE: usize = 256;

type Job = Box<dyn FnOnce(&mut App) + Send>;

/// What the command thread waits for: control never queues behind a wake.
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
    commands: Sender<Job>,
    shutdown: Sender<()>,
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

#[derive(Resource)]
pub(crate) struct Host {
    pub(crate) db: Database,
    pub(crate) credentials: Credentials,
    outbox: Vec<w::Notification>,
    pub(crate) events: broadcast::Sender<w::Notification>,
    epoch: String,
    sequence: u64,
    pub(crate) failed: Option<String>,
    pub(crate) closing: bool,
    pub(crate) device_streams: HashMap<String, tokio::sync::mpsc::Sender<w::DeviceOperation>>,
    pub(crate) models: Arc<dyn model::ModelSource>,
    pub(crate) consolidate: bool,
    pub(crate) consolidation_queue: HashSet<String>,
    /// How long a device operation may stay unanswered before the turn goes on.
    pub(crate) device_timeout: Duration,
}

/// How `Runtime::start_with` supplies models and consolidation.
///
/// The defaults are the running host: built-in providers and consolidation
/// on. Tests bring their own `ModelSource`, and switch consolidation with
/// the same option instead of a mutable flag on `Host`.
pub(crate) struct Options {
    pub(crate) models: Option<Arc<dyn model::ModelSource>>,
    pub(crate) consolidate: bool,
    pub(crate) device_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            models: None,
            consolidate: true,
            device_timeout: Duration::from_secs(120),
        }
    }
}

impl Host {
    pub(crate) fn ready(&self) -> Result<()> {
        if let Some(reason) = &self.failed {
            return Err(ConnectError::unavailable(reason.clone()));
        }
        if self.closing {
            return Err(ConnectError::unavailable("host is shutting down"));
        }
        Ok(())
    }

    pub(crate) fn cursor(&self) -> w::Cursor {
        w::Cursor {
            epoch: self.epoch.clone(),
            seq: self.sequence,
            ..Default::default()
        }
    }

    pub(crate) fn notify(
        &mut self,
        companion: Option<String>,
        client: Option<String>,
        event: w::notification::Event,
    ) {
        self.outbox.push(w::Notification {
            companion_id: companion,
            client_id: client,
            event: Some(event),
            ..Default::default()
        });
    }
}

#[derive(Component, Clone, Debug)]
pub(crate) struct Owner(pub(crate) w::Target);

impl Owner {
    pub(crate) fn companion(&self) -> &str {
        &self.0.companion_id
    }

    pub(crate) fn work(&self) -> &str {
        &self.0.work_id
    }

    pub(crate) fn is_work(&self) -> bool {
        !self.0.work_id.is_empty()
    }

    pub(crate) fn conversation_id(&self) -> String {
        chat::conversation_id(&self.0)
    }

    pub(crate) fn tool_access(&self) -> ToolAccess {
        let names = if self.is_work() {
            kernel::WORK_TOOLS
        } else {
            kernel::PARENT_TOOLS
        };
        ToolAccess::Only(names.iter().copied().map(str::to_owned).collect())
    }
}

/// Queues a client-visible warning about `companion`.
pub(crate) fn notice(host: &mut Host, companion: &str, code: &str, message: &str) {
    host.notify(
        Some(companion.into()),
        None,
        w::notification::Event::Notice(Box::new(w::Notice {
            code: code.into(),
            message: crate::truncate_chars(message, 500),
            ..Default::default()
        })),
    );
}

pub(crate) fn revoke_and_stop(world: &mut World, companion: Option<&str>) {
    let entities: Vec<Entity> = world
        .query::<(Entity, &Owner)>()
        .iter(world)
        .filter(|(_, owner)| companion.is_none_or(|id| owner.companion() == id))
        .map(|(entity, _)| entity)
        .collect();
    for entity in &entities {
        if let Some(binding) = world.get::<model::Binding>(*entity) {
            model::revoke(binding);
        }
        world
            .entity_mut(*entity)
            .remove::<(model::Binding, rig_ecs::model::Connection)>();
        world.trigger(StopTurn {
            entity: *entity,
            reason: Some("permissions changed".into()),
        });
    }
    let calls: Vec<Entity> = world
        .query::<(Entity, &ForAgent, &model::Binding)>()
        .iter(world)
        .filter(|(_, agent, _)| entities.contains(&agent.0))
        .map(|(call, _, binding)| {
            model::revoke(binding);
            call
        })
        .collect();
    for call in calls {
        world.entity_mut(call).remove::<model::Binding>();
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
        let db = Database::open(&state.join("host.sqlite3")).map_err(io_error)?;
        seed(&db).map_err(io_error)?;
        let credentials = Credentials::load(&db).await;
        let logs = DirStore::open(state.join("rig")).map_err(io_error)?;
        let (commands, command_rx) = bounded::<Job>(QUEUE);
        let (wake_tx, wake_rx) = bounded::<()>(1);
        let (shutdown, shutdown_rx) = bounded(1);
        let (ready_tx, initialized) = mpsc::sync_channel(1);
        let wake = wake_tx.clone();
        let host = Host {
            db,
            credentials: credentials.clone(),
            outbox: Vec::new(),
            events: broadcast::channel(EVENTS).0,
            epoch: Uuid::new_v4().to_string(),
            sequence: 0,
            failed: None,
            closing: false,
            device_streams: HashMap::new(),
            models: options.models.unwrap_or_else(|| Arc::new(model::Providers)),
            consolidate: options.consolidate,
            consolidation_queue: HashSet::new(),
            device_timeout: options.device_timeout,
        };
        let thread = std::thread::Builder::new()
            .name("ene-host-rig".into())
            .spawn(move || {
                let mut app = App::new();
                // One agent's work does not need a pool per core.
                app.add_plugins(TaskPoolPlugin {
                    task_pool_options: TaskPoolOptions {
                        max_total_threads: 3,
                        ..Default::default()
                    },
                })
                .insert_resource(Wake::new(move || {
                    // A wake is a hint that coalesces: `Wake` keeps its
                    // sender for the loop's life, so this never spins.
                    let _ = wake.try_send(());
                }))
                .insert_resource(host)
                .add_plugins((
                    AgentPlugin,
                    SessionLogPlugin,
                    kernel::KernelPlugin,
                ));
                app.finish();
                if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
                    time.set_max_delta(Duration::from_secs(60));
                }
                let result = open_session(app.world_mut(), Arc::new(logs))
                    .map_err(io_error)
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
                            app.world_mut().resource_mut::<Host>().closing = true;
                            revoke_and_stop(app.world_mut(), None);
                            app.world_mut().write_message(AppExit::Success);
                            tick(&mut app);
                            return match &app.world().resource::<Host>().failed {
                                Some(reason) => Err(ConnectError::unavailable(reason.clone())),
                                None => Ok(()),
                            };
                        }
                    }
                }
            })
            .map_err(io_error)?;
        initialized
            .recv()
            .map_err(|_| io_error("Rig kernel did not start"))?
            .map_err(io_error)?;
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
                    .resource::<Host>()
                    .ready()
                    .and_then(|()| operation(app));
                if result.as_ref().is_err_and(is_storage) {
                    fail(app.world_mut(), "state storage failed".into());
                }
                tick(app);
                let result = app.world().resource::<Host>().ready().and(result);
                let _ = reply.send(result);
            })
        })
        .await
    }

    /// Runs a read on the command thread without advancing a frame.
    ///
    /// Reads change nothing, so they need no `tick`: a tick would run the
    /// turn pipeline and move `Time<Virtual>` for a snapshot or a page of
    /// history.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.send(move |reply| {
            Box::new(move |app| {
                let result = app
                    .world()
                    .resource::<Host>()
                    .ready()
                    .and_then(|()| operation(app));
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
        self.handle.commands.try_send(job(reply)).map_err(|_| {
            ConnectError::resource_exhausted("host command queue is full or closed")
        })?;
        received
            .await
            .map_err(|_| ConnectError::unavailable("Rig kernel stopped"))?
    }

    pub(crate) async fn begin_shutdown(&self) {
        let _ = self
            .call(|app| {
                app.world_mut().resource_mut::<Host>().closing = true;
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
            .map_err(|_| ConnectError::unavailable("Rig kernel lock failed"))?
            .take();
        tokio::task::spawn_blocking(move || {
            thread.map_or(Ok(()), |thread| {
                let _ = shutdown.try_send(());
                thread
                    .join()
                    .map_err(|_| ConnectError::unavailable("Rig kernel panicked"))?
            })
        })
        .await
        .map_err(|_| ConnectError::unavailable("Rig kernel join failed"))?
    }
}

/// Waits for the next thing the command thread does: stop wins over work.
fn next(commands: &Receiver<Job>, wake: &Receiver<()>, shutdown: &Receiver<()>) -> Step {
    select_biased! {
        recv(shutdown) -> _ => Step::Stop,
        recv(commands) -> job => match job {
            Ok(job) => Step::Job(job),
            Err(_) => Step::Stop,
        },
        recv(wake) -> _ => Step::Wake,
        default(Duration::from_secs(1)) => Step::Wake,
    }
}

fn seed(database: &Database) -> Result<()> {
    if database.get::<w::Settings>("settings")?.is_none() {
        database.put(
            "settings",
            &w::Settings {
                revision: 1,
                ..Default::default()
            },
        )?;
    }
    if database.list::<CompanionRecord>("companion:")?.is_empty() {
        let record = CompanionRecord::seed(new_id(), now());
        database.put(&format!("companion:{}", record.companion.id), &record)?;
    }
    Ok(())
}

fn io_error(error: impl std::fmt::Display) -> StartError {
    std::io::Error::other(error.to_string()).into()
}

pub(crate) fn tick(app: &mut App) {
    app.update();
    if let Some(reason) = app.world().resource::<SessionLog>().failure() {
        fail(app.world_mut(), reason);
    }
    if app.world().resource::<Host>().failed.is_some() {
        app.world_mut().resource_mut::<Host>().outbox.clear();
        return;
    }
    let mut host = app.world_mut().resource_mut::<Host>();
    let outbox = std::mem::take(&mut host.outbox);
    for mut event in outbox {
        host.sequence += 1;
        event.cursor = w::Cursor {
            epoch: host.epoch.clone(),
            seq: host.sequence,
            ..Default::default()
        }
        .into();
        let _ = host.events.send(event);
    }
}

pub(crate) fn fail(world: &mut World, reason: String) {
    if world.resource::<Host>().failed.is_some() {
        return;
    }
    tracing::error!(%reason, "the host stopped");
    revoke_and_stop(world, None);
    let (events, epoch, sequence) = {
        let mut host = world.resource_mut::<Host>();
        host.failed = Some(reason.clone());
        host.outbox.clear();
        host.sequence += 1;
        (host.events.clone(), host.epoch.clone(), host.sequence)
    };
    let _ = events.send(w::Notification {
        cursor: w::Cursor {
            epoch,
            seq: sequence,
            ..Default::default()
        }
        .into(),
        event: Some(w::notification::Event::HostStorageFailed(Box::new(
            w::HostStorageFailed {
                failure: w::Failure {
                    code: "storage_error".into(),
                    message: crate::truncate_chars(&reason, 1024),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
        ))),
        ..Default::default()
    });
}

fn restore(app: &mut App) -> std::result::Result<(), StartError> {
    chat::restore(app.world_mut()).map_err(io_error)?;
    work::restore(app.world_mut()).map_err(io_error)?;
    client::restore(app.world_mut()).map_err(io_error)?;
    tick(app);
    let _ = mind::restore(app.world_mut());
    Ok(())
}

pub(crate) fn snapshot(app: &mut App, _subscription: Option<String>) -> Result<w::Snapshot> {
    let host = app.world().resource::<Host>();
    let health = w::HostHealth {
        ready: host.ready().is_ok(),
        recovery_required: host.failed.is_some(),
        stopping: host.closing,
        ..Default::default()
    };
    let works = host
        .db
        .list::<WorkRecord>("work:")?
        .into_iter()
        .map(|(_, record)| record.session)
        .collect();
    Ok(w::Snapshot {
        host_id: host.db.host_id()?,
        environment_id: host.db.environment_id()?,
        capabilities: w::HostCapabilities {
            limits: document(
                json!({"max_input_bytes": chat::MAX_INPUT, "max_companions": mind::MAX_COMPANIONS}),
            )
            .into(),
            features: [
                "history",
                "notifications",
                "chat",
                "connections",
                "credentials",
                "grants",
                "mind",
                "work",
                "clients",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            adapters: model::descriptors().into_iter().map(|p| p.id).collect(),
            transport: "/rpc".into(),
            ..Default::default()
        }
        .into(),
        cursor: host.cursor().into(),
        companions: mind::companions(&host.db)?,
        clients: client::list(host)?,
        settings: settings::settings(&host.db)?.into(),
        grants: settings::grants(&host.db, None)?,
        works,
        health: health.into(),
        ..Default::default()
    })
}

pub(crate) fn subscribe(
    app: &mut App,
    client_id: Option<&str>,
) -> Result<(
    w::Snapshot,
    tokio::sync::broadcast::Receiver<w::Notification>,
    String,
)> {
    if let Some(client_id) = client_id.filter(|id| !id.is_empty()) {
        validate_id(client_id)?;
        let _ = app
            .world()
            .resource::<Host>()
            .db
            .get::<w::Client>(&format!("client:{client_id}"))?;
    }
    let receiver = app.world().resource::<Host>().events.subscribe();
    let subscription = Uuid::new_v4().to_string();
    let snapshot = snapshot(app, Some(subscription.clone()))?;
    Ok((snapshot, receiver, subscription))
}
