mod access;
mod api;
mod auth;

use crate::{actor::Actor, config::HostArgs};
use anyhow::{Result, ensure};

pub async fn run(args: HostArgs) -> Result<()> {
    use tracing_subscriber::{EnvFilter, filter::filter_fn, prelude::*};
    let events = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter_fn(|metadata| {
            metadata.target().starts_with("ene_host")
        }));
    let _subscriber = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "ene_host=info".into()))
        .with(events)
        .try_init();
    ensure!(
        args.bind.ip().is_loopback(),
        "the listener must be local; use an authenticated reverse proxy for remote access"
    );
    let (state, runtime) = args.paths()?;
    let access = access::Access::acquire(&state, &runtime)?;
    for name in ["conversations", "avatars"] {
        std::fs::create_dir_all(state.join(name))?;
    }
    let (host, actor) = Actor::start(state).await?;
    let result = async {
        let listener = tokio::net::TcpListener::bind(args.bind).await?;
        let address = listener.local_addr()?;
        let token = access.publish(&format!("http://{address}/rpc"))?;
        let guard = auth::Guard::with_allowed_hosts(token, &args.allowed_hosts);
        tracing::info!(%address, "host ready");
        let shutdown = host.shutdown.clone();
        axum::serve(listener, api::router(host.clone(), guard))
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                shutdown.cancel();
            })
            .await?;
        Ok(())
    }
    .await;
    host.shutdown.cancel();
    actor.await?;
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        return;
    }
    let _signal = tokio::signal::ctrl_c().await;
}
