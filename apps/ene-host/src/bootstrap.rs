use crate::{
    api::state::App,
    infrastructure::{
        runtime::{self, Runtime},
        sqlite::Store,
    },
};
use std::{error::Error, path::PathBuf};
pub async fn initialize() -> Result<(Runtime, App), Box<dyn Error>> {
    let runtime = Runtime::acquire(&runtime::paths::runtime()?)?;
    let state = std::env::var_os("ENE_HOST_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(runtime::paths::state);
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
    Ok((runtime, app))
}

pub fn application(store: Store, token: String) -> App {
    let shutdown = tokio::sync::watch::channel(false).0;
    let application = ene_core::application::HostApplication::new(
        std::sync::Arc::new(store),
        std::sync::Arc::new(super::infrastructure::llm::registry::Registry::discover()),
        std::sync::Arc::new(super::infrastructure::credentials::Store::default()),
        shutdown.clone(),
    );
    App::new(application, token, shutdown)
}
