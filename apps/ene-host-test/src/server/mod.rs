//! Process startup, private discovery and graceful shutdown of the host API.
mod access;
mod rpc;

use self::rpc::router;
use crate::host::{self, Runtime};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;

/// Runs an authenticated loopback host. Its private discovery file is removed on exit.
/// The caller supplies termination; the binary uses `ctrlc` for native signal handling.
pub async fn serve(
    addr: SocketAddr,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !addr.ip().is_loopback() {
        return Err("only loopback listeners are supported".into());
    }
    let (state, runtime_path) = crate::paths()?;
    let access = access::Access::acquire(&state, &runtime_path)?;
    let runtime = Runtime::start(&state, host::factory()?)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    access.publish(&format!("http://{}", listener.local_addr()?))?;
    eprintln!(
        "ene-host-test: ready; discovery: {}",
        access.connection.display()
    );
    let shutdown = CancellationToken::new();
    let app = router(runtime.clone(), access.token.clone(), shutdown.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.await;
            shutdown.cancel();
        })
        .await?;
    runtime.shutdown().await?;
    drop(access);
    Ok(())
}
