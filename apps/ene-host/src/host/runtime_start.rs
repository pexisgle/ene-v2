//! Building a `Runtime`: opening the store, starting plugins, and
//! launching the kernel thread.

use super::kernel::{KernelLaunch, KernelResources, spawn_kernel_thread};
use super::*;

impl Runtime {
    pub(crate) async fn start(state: &Path) -> std::result::Result<Self, StartError> {
        Self::start_with(state, Options::default()).await
    }

    pub(crate) async fn start_with(
        state: &Path,
        options: Options,
    ) -> std::result::Result<Self, StartError> {
        let store = Store::open(&state.join("host.sqlite3"))?;
        store.companion_repo().seed_initial_companion()?;
        // A unit test has no operator to ask for a keyring, and the machine it
        // runs on may have none. Keeping the store in memory there makes the
        // tests independent of the platform backend; the binary still reads
        // the environment.
        let session_only = cfg!(test) || std::env::var_os("ENE_HOST_SESSION_CREDENTIALS").is_some();
        let credentials = Credentials::new(session_only);
        let log_path = state.join("rig");
        let logs = DirStore::open(&log_path).map_err(|source| StartError::SessionLog {
            path: log_path,
            source: Box::new(source),
        })?;
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
        let store_for_runtime = store.clone();
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
        .map_err(|_| StartError::Kernel)?;
        ready_rx
            .recv()
            .map_err(|_| StartError::Kernel)?
            .map_err(|_| StartError::Kernel)?;
        let voicevox = voice::voicevox::VoicevoxManager::with_auto_start(!cfg!(test));
        maybe_auto_start_voicevox(&store, &runtime_providers, &voicevox);
        Ok(Self {
            handle: Arc::new(Handle {
                commands,
                shutdown,
                thread: Mutex::new(Some(thread)),
            }),
            state_dir: state.to_path_buf(),
            store: store_for_runtime,
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

    pub(crate) fn store(&self) -> Store {
        self.store.clone()
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
}
