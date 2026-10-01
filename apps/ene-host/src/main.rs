mod api;
mod runtime;
mod store;

use clap::Parser;
use std::{error::Error, path::PathBuf};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    about = "ene local host",
    long_about = "ene local host: XDG_RUNTIME_DIR/ene-host/connection.json publishes the authenticated loopback endpoint. ENE_HOST_STATE_DIR overrides the SQLite state directory. Chat is not enabled yet."
)]
struct Args {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _args = Args::parse();
    let root = ene_shared::paths::runtime()?;
    let runtime = runtime::Runtime::acquire(&root)?;
    let state = if let Some(path) = std::env::var_os("ENE_HOST_STATE_DIR") {
        PathBuf::from(path)
    } else {
        let state = ene_shared::paths::state();
        if let Some(base) = state.parent() {
            std::fs::create_dir_all(base)?;
        }
        state
    };
    runtime::private_directory(&state)?;
    let database = state.join("host.sqlite3");
    let file = runtime::private_file(&database)?;
    drop(file);
    let store = store::Store::open(database)
        .await
        .map_err(|_| "database initialization failed")?;
    let app = api::App::new(store, runtime.token.clone());
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    runtime.publish(format!("http://{}/v1", listener.local_addr()?))?;
    eprintln!("ene-host: loopback service ready (history and notifications; chat not yet enabled)");
    let shutdown = app.shutdown.clone();
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                if let Ok(mut terminate) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
                    shutdown.send_replace(true);
                    return;
                }
            }
            let _ = tokio::signal::ctrl_c().await;
            shutdown.send_replace(true);
        })
        .await?;
    drop(runtime);
    Ok(())
}
