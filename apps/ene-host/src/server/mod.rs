//! Process startup, private discovery and graceful shutdown of the host API.
mod access;
mod auth;
mod history;
pub(crate) mod rpc;

use self::rpc::router;
use crate::host::Runtime;
use std::error::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub fn reset_history() -> Result<(), Box<dyn Error + Send + Sync>> {
    let (state, runtime) = crate::paths()?;
    let _access = access::Access::acquire(&state, &runtime)?;
    history::reset(&state, true)
}

#[tokio::main(flavor = "multi_thread")]
pub async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
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
    // The directories are the first thing the host touches, and a profile that
    // refuses them otherwise fails with a bare `PermissionDenied`.
    let access = access::Access::acquire(&state, &runtime_path).map_err(|error| {
        std::io::Error::other(format!(
            "cannot use the state directory {} or the runtime directory {} ({error}); \
             set ENE_HOST_STATE_DIR and ENE_HOST_RUNTIME_DIR to writable directories",
            state.display(),
            runtime_path.display()
        ))
    })?;
    history::reset(&state, false)?;
    let runtime = Runtime::start(&state).await?;
    // Access control is delegated to the OS or a fronting service
    // (docs/design/host-access.md): the bind address is explicit, and the
    // Owner Bearer stays mandatory even on a non-loopback listener.
    let bind = std::env::var("ENE_HOST_BIND").unwrap_or_else(|_| "127.0.0.1:0".into());
    let listener = TcpListener::bind(&bind).await?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        eprintln!(
            "ene-host: WARNING listening on {local}; Owner Bearer remains required. \
             Do not expose this port without the external access control described in docs/design/host-access.md"
        );
    }
    access.publish(&format!("http://{local}/rpc"))?;
    let guard = auth::Guard::new(access.token.clone());
    if !guard.allowed().is_empty() {
        eprintln!(
            "ene-host: trusting Host/Origin authorities {} besides loopback; the Owner Bearer is still required",
            guard
                .allowed()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    eprintln!("ene-host: authenticated service ready on {local}");
    let shutdown = CancellationToken::new();
    let app = router(runtime.clone(), guard, shutdown.clone());
    let running = runtime.clone();
    // `axum::serve` drives each connection through `hyper_util`'s
    // `server::conn::auto::Builder`, so it accepts HTTP/2 prior knowledge on a
    // plain socket as well as HTTP/1.1 (tech-stack §G, h2c). That is what the
    // bidirectional VoiceService streams need, and why this does not pin the
    // listener to HTTP/1.1.
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_shutdown().await;
            shutdown.cancel();
            running.begin_shutdown().await;
        })
        .await?;
    let result = runtime.shutdown().await;
    drop(access);
    result.map_err(Into::into)
}

async fn wait_for_shutdown() {
    if std::env::var_os("ENE_HOST_EXIT_ON_STDIN_EOF").is_none() {
        wait_for_signal().await;
        return;
    }

    // The desktop holds the write end until it exits, including on a crash.
    let (closed, wait) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        let _ = closed.send(());
    });
    let _ = wait.await;
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
