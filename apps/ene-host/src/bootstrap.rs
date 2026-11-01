use crate::{
    api::state::App,
    infrastructure::{
        paths,
        runtime::{self, Runtime},
        sqlite::Store,
    },
};
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
    app.application
        .initialize()
        .await
        .map_err(|_| "credential initialization failed")?;
    app.devices
        .initialize()
        .await
        .map_err(|_| "device initialization failed")?;
    app.application.restore_plugins().await?;
    Ok((runtime, app))
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
    App::new(application, token).with_device_store(devices)
}
