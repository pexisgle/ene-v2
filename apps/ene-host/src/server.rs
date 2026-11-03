use crate::{api, bootstrap};
use std::error::Error;
use tokio::net::TcpListener;

/// `ene-host` takes no arguments; configuration comes from the environment.
const USAGE: &str = "ene-host takes no arguments.\
\nXDG_RUNTIME_DIR/ene-host/connection.json publishes the authenticated loopback endpoint.\
\nENE_HOST_STATE_DIR overrides the SQLite state directory.\
\nENE_PLUGIN_DIR overrides the runtime-installable plugin directory.";

pub fn run() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve());
    // Secret Service calls run on bounded blocking slots. A timed-out call
    // cannot be aborted by Tokio and must not keep process shutdown waiting forever.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    result
}

async fn serve() -> Result<(), Box<dyn Error>> {
    if let Some(arg) = std::env::args_os().nth(1) {
        if arg == "-h" || arg == "--help" {
            println!("{USAGE}");
            return Ok(());
        }
        eprintln!("{USAGE}");
        return Err("invalid arguments".into());
    }
    let (runtime, app) = bootstrap::initialize().await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    runtime.publish(format!("http://{}/rpc", listener.local_addr()?))?;
    eprintln!("ene-host: authenticated loopback service ready");
    let shutdown = app.application.clone();
    let application = app.application.clone();
    let devices = app.devices.clone();
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                if let Ok(mut terminate) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
                    shutdown.begin_shutdown();
                    return;
                }
            }
            let _ = tokio::signal::ctrl_c().await;
            shutdown.begin_shutdown();
        })
        .await?;
    devices.stop().await;
    application.stop().await?;
    drop(runtime);
    Ok(())
}
