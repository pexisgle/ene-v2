use crate::{
    api::state::App,
    infrastructure::{
        paths,
        runtime::{self, Runtime},
        sqlite::Store,
    },
};
use rig_ecs::prelude::AppToolsExt;
use std::{error::Error, path::PathBuf};
pub async fn initialize() -> Result<(Runtime, App), Box<dyn Error>> {
    let runtime = Runtime::acquire(&paths::runtime()?)?;
    let state = std::env::var_os("ENE_HOST_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(paths::state);
    if let Some(base) = state.parent() {
        std::fs::create_dir_all(base)?;
    }
    runtime::private_directory(&state)?;
    let database = state.join("host.sqlite3");
    drop(runtime::private_file(&database)?);
    let store = Store::open(database)
        .await
        .map_err(|_| "database initialization failed")?;
    let app = application(store, runtime.token.clone());
    let rig_dir = state.join("rig");
    runtime::private_directory(&rig_dir)?;
    attach_kernel(&app, rig_dir)?;
    app.application
        .initialize()
        .await
        .map_err(|_| "credential initialization failed")?;
    app.application.restore_plugins().await?;
    Ok((runtime, app))
}

/// Starts the kernel with the host-bound mind tools and the task that answers
/// them. The kernel still owns turns and tool execution; the host owns the
/// canonical SQLite store and the input source binding.
pub(crate) fn attach_kernel(app: &App, rig_dir: PathBuf) -> Result<(), Box<dyn Error>> {
    let (calls, received) = tokio::sync::mpsc::unbounded_channel();
    let bridge = std::sync::Arc::new(crate::rig::mind::MindBridge::new(calls));
    let configure = bridge.clone();
    let (kernel, events) = crate::rig::Runtime::start_with(rig_dir, move |app| {
        app.insert_resource((*configure).clone());
        app.add_tool(crate::rig::mind::MindRecall(configure.clone()));
        app.add_tool(crate::rig::mind::MindRead(configure.clone()));
        app.add_tool(crate::rig::mind::MindUpdate(configure));
    })
    .map_err(|_| "the Rig execution kernel failed to start")?;
    crate::application::mind::spawn_mind_tools(app.application.clone(), received);
    app.application.attach_rig(kernel, events);
    Ok(())
}

pub fn application(store: Store, token: String) -> App {
    let plugins = std::sync::Arc::new(super::infrastructure::plugins::Manager::discover(
        store.clone(),
    ));
    application_with_plugins(store, token, plugins)
}
pub(crate) fn application_with_plugins(
    store: Store,
    token: String,
    plugins: std::sync::Arc<super::infrastructure::plugins::Manager>,
) -> App {
    let shutdown = tokio::sync::watch::channel(false).0;
    let devices = store.clone();
    let providers: std::sync::Arc<dyn crate::llm::ProviderRegistry> = std::sync::Arc::new(
        super::infrastructure::llm::registry::Registry::with_plugins(Some(plugins.clone())),
    );
    let application = crate::application::HostApplication::with_plugins(
        std::sync::Arc::new(store),
        providers,
        std::sync::Arc::new(super::infrastructure::credentials::Store::default()),
        shutdown,
        Some(plugins.clone()),
    );
    plugins.attach_inference(application.inference());
    let mut app = App::new(application, token);
    app.devices = crate::devices::Devices::new(Some(devices));
    app
}
