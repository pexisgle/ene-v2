mod api;
mod bootstrap;
mod infrastructure;

use clap::Parser;
use std::error::Error;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    about = "ene local host",
    long_about = "ene local host: XDG_RUNTIME_DIR/ene-host/connection.json publishes the authenticated loopback endpoint. ENE_HOST_STATE_DIR overrides the SQLite state directory. ENE_PROVIDER_DIR overrides the runtime-installable provider plugin directory."
)]
struct Args {
    /// Export the generated OpenAPI contract without starting a host or opening its database.
    #[arg(long)]
    print_openapi: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if args.print_openapi {
        println!(
            "{}",
            serde_json::to_string_pretty(&api::openapi::specification())?
        );
        return Ok(());
    }
    let (runtime, app) = bootstrap::initialize().await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    runtime.publish(format!("http://{}/v1", listener.local_addr()?))?;
    eprintln!("ene-host: authenticated loopback service ready");
    let shutdown = app.shutdown.clone();
    let application = app.application.clone();
    let mutations = app.clone();
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
    mutations.finish_mutations().await;
    application.stop().await?;
    drop(runtime);
    Ok(())
}
