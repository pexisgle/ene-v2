//! Process startup, private discovery and graceful shutdown of the host API.
mod access;
mod auth;
pub(crate) mod rpc;

use self::rpc::router;
use crate::host::Runtime;
use std::error::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const USAGE: &str = "ene-host takes no arguments.\n\
The endpoint and Owner token are published in the runtime directory's connection.json.\n\
ENE_HOST_STATE_DIR overrides the SQLite/rig directory.\n\
ENE_HOST_RUNTIME_DIR overrides the discovery/lock directory.";

pub fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    serve()
}

#[tokio::main(flavor = "multi_thread")]
async fn serve() -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Some(argument) = std::env::args_os().nth(1) {
        if argument == "-h" || argument == "--help" {
            println!("{USAGE}");
            return Ok(());
        }
        eprintln!("{USAGE}");
        return Err("invalid arguments".into());
    }
    let (state, runtime_path) = crate::paths()?;
    // The host runs for one desktop: its log goes to stderr, warnings and
    // above unless `RUST_LOG` asks for more.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let access = access::Access::acquire(&state, &runtime_path)?;
    let runtime = Runtime::start(&state).await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    access.publish(&format!("http://{}/rpc", listener.local_addr()?))?;
    eprintln!("ene-host: authenticated loopback service ready");
    let shutdown = CancellationToken::new();
    let app = router(runtime.clone(), access.token.clone(), shutdown.clone());
    let running = runtime.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            shutdown.cancel();
            running.begin_shutdown().await;
        })
        .await?;
    let result = runtime.shutdown().await;
    drop(access);
    result.map_err(Into::into)
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
