//! The host: one headless Rig app on a thread of its own, the SQLite state and
//! the notification outbox.
//!
//! Every RPC operation is serialized onto the app thread, so SQLite, the ECS
//! world and the conversation log never race. Reads do not advance the agent
//! loop; mutations do, and the frame's notifications are flushed after it.
pub(crate) mod agent;
pub(crate) mod autonomy;
pub(crate) mod background;
pub(crate) mod chat;
pub(crate) mod client;
pub(crate) mod compaction;
pub(crate) mod db;
pub(crate) mod delegation;
pub(crate) mod error;
pub(crate) mod eyes;
pub(crate) mod json;
pub(crate) mod mcp;
pub(crate) mod mind;
pub(crate) mod model;
pub(crate) mod net;
pub(crate) mod notify;
pub(crate) mod permission;
pub(crate) mod providers;
pub(crate) mod resources;
pub(crate) mod settings;
pub(crate) mod tools;
pub(crate) mod util;
pub(crate) mod voice;
pub(crate) mod web_search;

#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod wire;
pub(crate) mod work;

use self::{
    db::Store,
    error::{HostError, Result},
    model::Credentials,
    notify::Notifier,
    permission::Approvals,
    resources::{DeviceHub, DeviceTimeout, HostRuntime, HostState, Options, Perception, Waiting},
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

/// Locks `mutex` and takes the guard back from a poisoned one. A panic that
/// unwound while a lock was held leaves the guarded value whole, so the host
/// recovers the lock instead of failing every later access to it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

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
    state_dir: std::path::PathBuf,
    credentials: Credentials,
    hub: DeviceHub,
    providers: providers::Providers,
    voicevox: voice::voicevox::VoicevoxManager,
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
        store
            .companion_repo()
            .seed_initial_companion()
            .map_err(boxed)?;
        // A unit test has no operator to ask for a keyring, and the machine it
        // runs on may have none. Keeping the store in memory there makes the
        // tests independent of the platform backend; the binary still reads
        // the environment.
        let session_only = cfg!(test) || std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some();
        let credentials = Credentials::new(session_only);
        let logs = DirStore::open(state.join("rig")).map_err(boxed)?;
        let (commands, command_rx) = crossbeam_channel::bounded::<Job>(QUEUE);
        let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let (shutdown, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let hub = DeviceHub::default();
        // Plugin packages are read and verified once, before the kernel runs,
        // so every path that resolves a connection stays synchronous. The
        // instances the stored connections reference start here too, for the
        // same reason: a frame cannot await a load.
        let providers = providers::Providers::load(state);
        let runtime_providers = providers.clone();
        // An operator who installed a plugin that did not load has no other
        // way to find out: the connection it named simply resolves to nothing.
        let installed: Vec<&str> = providers.adapters().collect();
        if !installed.is_empty() {
            tracing::info!(plugins = %installed.join(","), "provider plugins loaded");
        }
        let ((), initial_plugin_tools, (models, catalog_refresh)) = tokio::join!(
            providers.start_used(&store),
            providers::extensions::discover_plugin_tools(&providers, &store),
            model::catalog::Refresh::initial()
        );
        let thread = spawn_kernel_thread(KernelLaunch {
            resources: KernelResources {
                store: store.clone(),
                providers,
                credentials: credentials.clone(),
                models,
                catalog_refresh,
                host_runtime: tokio::runtime::Handle::current(),
                hub: hub.clone(),
                device_timeout: options.device_timeout,
                wake: wake_tx,
                logs,
            },
            initial_plugin_tools,
            ready_tx,
            command_rx,
            wake_rx,
            shutdown_rx,
        })
        .map_err(boxed)?;
        ready_rx
            .recv()
            .map_err(|_| boxed("Rig kernel did not start"))?
            .map_err(boxed)?;
        let voicevox = voice::voicevox::VoicevoxManager::with_auto_start(!cfg!(test));
        maybe_auto_start_voicevox(&store, &runtime_providers, &voicevox);
        Ok(Self {
            handle: Arc::new(Handle {
                commands,
                shutdown,
                thread: Mutex::new(Some(thread)),
            }),
            state_dir: state.to_path_buf(),
            credentials,
            hub,
            providers: runtime_providers,
            voicevox,
        })
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn providers(&self) -> providers::Providers {
        self.providers.clone()
    }

    pub(crate) fn credentials(&self) -> Credentials {
        self.credentials.clone()
    }

    pub(crate) fn hub(&self) -> DeviceHub {
        self.hub.clone()
    }

    pub(crate) fn voicevox(&self) -> voice::voicevox::VoicevoxManager {
        self.voicevox.clone()
    }

    /// Tries to enqueue `operation` immediately. Accepted jobs advance one
    /// frame and flush notifications even if the reply future is dropped.
    pub(crate) fn call<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut App) -> Result<T> + Send + 'static,
    {
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
    }

    /// Tries to enqueue `operation` immediately. Accepted jobs run without
    /// advancing the agent loop even if the reply future is dropped.
    pub(crate) fn read<T: Send + 'static, F>(
        &self,
        operation: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut App) -> Result<T> + Send + 'static,
    {
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
    }

    fn send<T: Send + 'static, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T>> + Send + use<T, F>
    where
        F: FnOnce(oneshot::Sender<Result<T>>) -> Job,
    {
        let (reply, received) = oneshot::channel();
        let submitted = self
            .handle
            .commands
            .try_send(job(reply))
            .map_err(|_| HostError::Exhausted(("host command queue is full or closed").into()));
        async move {
            submitted?;
            received
                .await
                .map_err(|_| HostError::Unavailable(("Rig kernel stopped").into()))?
        }
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
        self.voicevox.stop().await;
        let shutdown = self.handle.shutdown.clone();
        let thread = self
            .handle
            .thread
            .lock()
            .map_err(|_| HostError::Internal(("Rig kernel lock failed").into()))?
            .take();
        tokio::task::spawn_blocking(move || {
            thread.map_or(Ok(()), |thread| {
                let _ = shutdown.try_send(());
                thread
                    .join()
                    .map_err(|_| HostError::Internal(("Rig kernel panicked").into()))?
            })
        })
        .await
        .map_err(|_| HostError::Internal(("Rig kernel join failed").into()))?
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
    if world
        .get_resource::<HostState>()
        .is_some_and(|state| state.failed.is_some())
    {
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
    if let Some(mut state) = world.get_resource_mut::<HostState>() {
        state.failed = Some(reason.clone());
    }
    if let Some(mut notifier) = world.get_resource_mut::<Notifier>() {
        notifier.fail(&reason);
    }
}

/// Restores state that must not wait for the ECS frame: stale approvals and
/// operations whose answers can no longer arrive.
fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    store.approval_repo().expire_pending_approvals()?;
    store.client_repo().mark_unanswered_operations_uncertain()?;
    Ok(())
}

/// Restores what needs a restorable session: the agents, their models and the
/// capability tools. It runs after the first frame, so the kernel's restore and
/// its startup systems are done before the host adopts or retires an agent.
fn restore_after_first_frame(world: &mut World) -> Result<()> {
    agent::reconcile(world)?;
    // The parents exist once the reconciliation has bound them, so publishing
    // now puts a restored active client's tools on every parent before the
    // first turn.
    client::dynamic::init(world);
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
    let companions: Vec<w::Companion> = store
        .companion_repo()
        .companions()?
        .iter()
        .map(wire::companion)
        .collect();
    let activations = store.client_repo().activations()?;
    let active_companion_id = activations
        .iter()
        .find(|activation| activation.client_id.is_some())
        .map(|activation| activation.companion_id.clone());
    let mut work_sessions = Vec::new();
    for companion in &companions {
        for session in store.work_repo().work_sessions(&companion.id)? {
            work_sessions.push(wire::work_session(&session));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let work_sessions = work_sessions
        .into_iter()
        .filter(|session| seen.insert(session.id.clone()))
        .collect();
    let clients = client::clients(&store, hub)?;
    let loaded = app
        .world()
        .get_resource::<providers::Providers>()
        .cloned()
        .unwrap_or_default();
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
                "diagnostics",
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            adapters: settings::providers(&loaded)
                .into_iter()
                .map(|provider| provider.adapter)
                .collect(),
            transport: "/rpc".into(),
            ..Default::default()
        }
        .into(),
        settings: Some(wire::settings(&store.settings_repo().settings()?)).into(),
        companions,
        work_sessions,
        active_companion_id,
        pending_approvals: store
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)?
            .iter()
            .map(wire::approval)
            .collect(),
        clients,
        cursor: Some(notifier.cursor()).into(),
        policy_rules: store
            .policy_repo()
            .policy_rules()?
            .iter()
            .map(wire::policy_rule)
            .collect(),
        activations: activations.iter().map(wire::activation).collect(),
        ..Default::default()
    })
}

/// The state a subscriber starts from: the snapshot, the live receiver and,
/// when the cursor is resumable, the events after it.
///
/// The replay and the receiver are read in one host operation. Two operations
/// would let a tick flush between them, and an event flushed there would be in
/// neither the replay nor the receiver.
pub(crate) fn subscribe(
    app: &mut App,
    client_id: Option<&str>,
    cursor: Option<&w::Cursor>,
) -> Result<(
    w::Snapshot,
    broadcast::Receiver<w::Notification>,
    Option<Vec<w::Notification>>,
)> {
    let store = app.world().resource::<Store>().clone();
    if let Some(client_id) = client_id.filter(|id| !id.is_empty()) {
        util::validate_id(client_id)?;
        if store.client_repo().client(client_id)?.is_none() {
            return Err(HostError::NotFound("client not found".into()));
        }
    }
    let (receiver, replayed) = {
        let notifier = app.world().resource::<Notifier>();
        (
            notifier.subscribe(),
            cursor.and_then(|cursor| notifier.replay(cursor)),
        )
    };
    Ok((snapshot(app)?, receiver, replayed))
}

struct KernelResources {
    store: Store,
    providers: providers::Providers,
    credentials: Credentials,
    models: rig_ecs::model::Models,
    catalog_refresh: model::catalog::Refresh,
    host_runtime: tokio::runtime::Handle,
    hub: DeviceHub,
    device_timeout: Duration,
    wake: crossbeam_channel::Sender<()>,
    logs: DirStore,
}

struct KernelLaunch {
    resources: KernelResources,
    initial_plugin_tools: Vec<(String, ene_plugin::PluginToolDefinition)>,
    ready_tx: mpsc::SyncSender<Result<()>>,
    command_rx: crossbeam_channel::Receiver<Job>,
    wake_rx: crossbeam_channel::Receiver<()>,
    shutdown_rx: crossbeam_channel::Receiver<()>,
}

fn spawn_kernel_thread(launch: KernelLaunch) -> std::io::Result<JoinHandle<Result<()>>> {
    std::thread::Builder::new()
        .name("ene-host-rig".into())
        .spawn(move || {
            let mut app = build_kernel_app(launch.resources);
            let result = (|| -> Result<()> {
                restore(app.world_mut())?;
                providers::extensions::register_plugin_tools(
                    app.world_mut(),
                    launch.initial_plugin_tools,
                );
                tick(&mut app);
                restore_after_first_frame(app.world_mut())?;
                Ok(())
            })();
            let ok = result.is_ok();
            let _ = launch.ready_tx.send(result);
            if !ok {
                return Ok(());
            }
            run_kernel_loop(
                &mut app,
                &launch.command_rx,
                &launch.wake_rx,
                &launch.shutdown_rx,
            )
        })
}

fn build_kernel_app(res: KernelResources) -> App {
    let mut app = App::new();
    app.set_error_handler(bevy_ecs::error::warn)
        .add_plugins(TaskPoolPlugin {
            task_pool_options: TaskPoolOptions {
                max_total_threads: 4,
                ..Default::default()
            },
        })
        .insert_resource(res.store)
        .insert_resource(res.providers)
        .insert_resource(res.credentials)
        .insert_resource(res.models)
        .insert_resource(res.catalog_refresh)
        .insert_resource(HostRuntime(res.host_runtime))
        .insert_resource(res.hub)
        .insert_resource(HostState::default())
        .insert_resource(Notifier::new())
        .insert_resource(Waiting::default())
        .insert_resource(Perception::default())
        .insert_resource(Approvals::default())
        .insert_resource(DeviceTimeout(res.device_timeout))
        .insert_resource(rig_ecs::tasks::Wake::new(move || {
            let _ = res.wake.try_send(());
        }))
        .insert_resource(SessionStore::new(res.logs))
        .add_plugins((
            AgentPlugin,
            SessionLogPlugin,
            agent::AgentPlugin,
            permission::GatePlugin,
            client::ClientPlugin,
            tools::sync::ToolsSyncPlugin,
            work::WorkPlugin,
            mind::MindPlugin,
            mcp::McpPlugin,
            providers::extensions::ExtensionPlugin,
            autonomy::AutonomyPlugin,
            compaction::CompactionPlugin,
            voice::VoicePlugin,
        ))
        .add_systems(Update, model::catalog::refresh.before(AgentLoop));
    app.finish();
    if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
        time.set_max_delta(Duration::from_secs(60));
    }
    app
}

fn run_kernel_loop(
    app: &mut App,
    command_rx: &crossbeam_channel::Receiver<Job>,
    wake_rx: &crossbeam_channel::Receiver<()>,
    shutdown_rx: &crossbeam_channel::Receiver<()>,
) -> Result<()> {
    loop {
        match next(command_rx, wake_rx, shutdown_rx) {
            Step::Job(job) => job(app),
            Step::Wake => tick(app),
            Step::Stop => {
                app.world_mut().resource_mut::<HostState>().closing = true;
                app.world_mut().write_message(AppExit::Success);
                tick(app);
                return match &app.world().resource::<HostState>().failed {
                    Some(reason) => Err(HostError::Unavailable(reason.clone())),
                    None => Ok(()),
                };
            }
        }
    }
}

fn maybe_auto_start_voicevox(
    store: &Store,
    providers: &providers::Providers,
    voicevox: &voice::voicevox::VoicevoxManager,
) {
    if !cfg!(test)
        && let Ok(settings) = store.settings_repo().settings()
        && let Some(vv) = settings.connections.iter().find(|conn| {
            conn.enabled
                && conn.kind == settings::ConnectionKind::Tts
                && conn.endpoint().adapter.eq_ignore_ascii_case("voicevox")
                && !model::is_sakura_endpoint(&conn.endpoint().endpoint)
                && providers.get("voicevox").is_none()
        })
    {
        let mgr = voicevox.clone();
        let base = vv.endpoint().endpoint.clone();
        let options = vv.endpoint().options.clone();
        tokio::spawn(async move {
            if let Err(error) = mgr.ensure_running(&base, &options).await {
                tracing::warn!(%error, "could not auto-start VOICEVOX engine at startup");
            }
        });
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn discarded_replies_leave_bounded_nonblocking_actor_jobs() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let blocked = runtime.read(move |_| {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(())
        });
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        let completed = Arc::new(AtomicUsize::new(0));
        for _ in 0..QUEUE - 1 {
            let completed = completed.clone();
            drop(runtime.read(move |_| {
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
        }
        let last = runtime.read({
            let completed = completed.clone();
            move |_| {
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        assert!(matches!(
            runtime
                .call::<(), _>(|_| panic!("a full queue must reject this job"))
                .await,
            Err(HostError::Exhausted(_))
        ));
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        blocked.await.unwrap();
        last.await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), QUEUE);
        runtime.shutdown().await.unwrap();
        assert!(matches!(
            runtime
                .read::<(), _>(|_| panic!("a closed queue must reject this job"))
                .await,
            Err(HostError::Exhausted(_))
        ));
    }

    #[tokio::test]
    async fn storage_errors_with_discarded_replies_still_stop_the_host() {
        for mutation in [false, true] {
            let state = tempfile::tempdir().unwrap();
            let runtime = Runtime::start(state.path()).await.unwrap();
            runtime
                .read(|app| {
                    app.world()
                        .resource::<Store>()
                        .conn()?
                        .execute_batch("DROP TABLE clients")?;
                    Ok(())
                })
                .await
                .unwrap();
            let broken = |app: &mut App| {
                app.world()
                    .resource::<Store>()
                    .client_repo()
                    .client("c1")
                    .map(|_| ())
                    .map_err(HostError::from)
            };
            if mutation {
                drop(runtime.call(broken));
            } else {
                drop(runtime.read(broken));
            }
            assert!(matches!(
                runtime
                    .read::<(), _>(|_| panic!("a failed host must refuse work"))
                    .await,
                Err(HostError::Unavailable(_))
            ));
            assert!(matches!(
                runtime.shutdown().await,
                Err(HostError::Unavailable(_))
            ));
        }
    }

    #[tokio::test]
    async fn fresh_runtime_seeds_ene_once_and_does_not_resurrect_after_delete() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        let snap = runtime.read(snapshot).await.unwrap();
        assert!(
            snap.companions
                .iter()
                .any(|c| c.id == "ene" && c.name == "ene"),
            "fresh host must include seeded ene companion"
        );
        runtime
            .call(|app| {
                super::mind::delete(app.world_mut(), "ene")?;
                Ok(())
            })
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();

        let restarted = Runtime::start(state.path()).await.unwrap();
        let snap = restarted.read(snapshot).await.unwrap();
        assert!(
            snap.companions.is_empty(),
            "deleted initial companion must stay deleted after restart"
        );
        restarted.shutdown().await.unwrap();
    }
}
