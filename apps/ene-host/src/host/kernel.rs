//! Starting the kernel thread and subscribing a client to notifications.

use super::snapshot::snapshot;
use super::*;

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
        ene_domain::validate_id(client_id)?;
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

pub(super) struct KernelResources {
    pub(super) store: Store,
    pub(super) providers: providers::Providers,
    pub(super) credentials: Credentials,
    pub(super) models: rig_ecs::model::Models,
    pub(super) catalog_refresh: model::catalog::Refresh,
    pub(super) host_runtime: tokio::runtime::Handle,
    pub(super) hub: DeviceHub,
    pub(super) device_timeout: Duration,
    pub(super) wake: crossbeam_channel::Sender<()>,
    pub(super) logs: DirStore,
}

pub(super) struct KernelLaunch {
    pub(super) resources: KernelResources,
    pub(super) initial_plugin_tools: Vec<(String, ene_plugin::PluginToolDefinition)>,
    pub(super) ready_tx: mpsc::SyncSender<Result<()>>,
    pub(super) command_rx: crossbeam_channel::Receiver<Job>,
    pub(super) wake_rx: crossbeam_channel::Receiver<()>,
    pub(super) shutdown_rx: crossbeam_channel::Receiver<()>,
}

pub(super) fn spawn_kernel_thread(launch: KernelLaunch) -> std::io::Result<JoinHandle<Result<()>>> {
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
            let _ready = launch.ready_tx.send(result);
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
            let _wake = res.wake.try_send(());
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
