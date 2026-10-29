use crate::{api, bootstrap};
use clap::Parser;
use std::error::Error;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    about = "ene local host",
    long_about = "ene local host: XDG_RUNTIME_DIR/ene-host/connection.json publishes the authenticated loopback endpoint. ENE_HOST_STATE_DIR overrides the SQLite state directory. ENE_PLUGIN_DIR overrides the runtime-installable plugin directory. ENE_PLUGIN_RUNNER_BIN overrides the isolated plugin runner executable."
)]
struct Args {}

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
    let _args = Args::parse();
    let (runtime, mut app) = bootstrap::initialize().await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let web_origin = std::env::var("ENE_WEB_ORIGIN")
        .unwrap_or_else(|_| format!("http://{}", listener.local_addr().expect("bound listener")));
    let parsed = url::Url::parse(&web_origin)?;
    if parsed.origin().ascii_serialization() != web_origin
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
        || !matches!(parsed.scheme(), "http" | "https")
    {
        return Err("invalid ENE_WEB_ORIGIN".into());
    }
    app.web_origin = Some(web_origin.into());
    runtime.publish(format!("http://{}/rpc", listener.local_addr()?))?;
    eprintln!("ene-host: authenticated loopback service ready");
    let shutdown = app.shutdown.clone();
    let application = app.application.clone();
    let devices = app.devices.clone();
    let audio = app.audio.clone();
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
    audio.shutdown().await;
    devices.stop().await;
    application.stop().await?;
    drop(runtime);
    Ok(())
}
