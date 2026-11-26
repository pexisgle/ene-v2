//! Starting the kernel thread and subscribing a client to notifications.

use super::*;

pub(super) struct KernelResources {
    pub(super) services: Arc<HostServices>,
    pub(super) catalog_refresh: model::catalog::Refresh,
    pub(super) host_runtime: tokio::runtime::Handle,
    pub(super) device_timeout: Duration,
    pub(super) wake: crossbeam_channel::Sender<()>,
    pub(super) logs: DirStore,
}

pub(super) struct KernelLaunch {
    pub(super) resources: KernelResources,
    pub(super) initial_plugin_tools: Vec<(String, ene_plugin::PluginToolDefinition)>,
    pub(super) ready_tx: oneshot::Sender<Result<()>>,
    pub(super) command_rx: crossbeam_channel::Receiver<Job>,
    pub(super) wake_rx: crossbeam_channel::Receiver<()>,
    pub(super) shutdown_rx: crossbeam_channel::Receiver<()>,
}

struct MarkClosed(HostState);

impl Drop for MarkClosed {
    fn drop(&mut self) {
        self.0.close();
    }
}

pub(super) fn spawn_kernel_thread(launch: KernelLaunch) -> std::io::Result<JoinHandle<Result<()>>> {
    std::thread::Builder::new()
        .name("ene-host-rig".into())
        .spawn(move || {
            let services = Arc::clone(&launch.resources.services);
            let _close = MarkClosed(services.health.clone());
            let mut app = build_kernel_app(launch.resources);
            let result = (|| -> Result<()> {
                restore(&services.store)?;
                providers::extensions::register_plugin_tools(
                    app.world_mut(),
                    launch.initial_plugin_tools,
                );
                tick(&mut app);
                restore_after_first_frame(app.world_mut())?;
                Ok(())
            })();
            let ok = result.is_ok();
            if launch.ready_tx.send(result).is_err() || !ok {
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
        .insert_resource(res.services.store.clone())
        .insert_resource(res.services.providers.clone())
        .insert_resource(res.services.credentials.clone())
        .insert_resource(rig_ecs::model::Models(rig_core::catalog::Connector::new(
            res.services.catalog.read().clone(),
        )))
        .insert_resource(res.catalog_refresh)
        .insert_resource(HostRuntime(res.host_runtime))
        .insert_resource(res.services.hub.clone())
        .insert_resource(res.services.health.clone())
        .insert_resource(res.services.notifier.clone())
        .insert_resource(res.services.voice.clone())
        .insert_resource(res.services.mcp.clone())
        .insert_resource(Waiting::default())
        .insert_resource(Perception::default())
        .insert_resource(Approvals::default())
        .init_resource::<delegation::DeliveredClaims>()
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
            providers::character::CharacterPlugin,
            autonomy::AutonomyPlugin,
            compaction::CompactionPlugin,
            voice::VoicePlugin,
        ))
        .add_systems(Update, model::catalog::apply_refresh.before(AgentLoop));
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
                app.world().resource::<HostState>().close();
                app.world_mut().write_message(AppExit::Success);
                tick(app);
                return match app.world().resource::<HostState>().failure() {
                    Some(reason) => Err(HostError::Unavailable(reason.clone())),
                    None => Ok(()),
                };
            }
        }
    }
}
