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
    let config = crate::config::HostConfig::from_env()?;
    let _access = access::Access::acquire(&config.state_dir, &config.runtime_dir)?;
    history::reset(&config.state_dir, true)
}

#[tokio::main(flavor = "multi_thread")]
pub async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Every setting is read and validated here, once, before anything runs.
    let config = crate::config::HostConfig::from_env()?;
    let state = config.state_dir.clone();
    let runtime_path = config.runtime_dir.clone();
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
    let listener = TcpListener::bind(config.bind).await?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        tracing::warn!(
            %local,
            "listening on non-loopback address; Owner Bearer remains required; \
             do not expose this port without the external access control described in docs/design/host-access.md"
        );
    }
    access.publish(&format!("http://{local}/rpc"))?;
    let guard = auth::Guard::with_allowed_hosts(access.token.clone(), &config.allowed_hosts);
    if !guard.allowed().is_empty() {
        let allowed = guard
            .allowed()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        tracing::warn!(
            %allowed,
            "trusting Host/Origin authorities besides loopback; the Owner Bearer is still required"
        );
    }
    tracing::info!(%local, "authenticated service ready");
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
            wait_for_shutdown(config.exit_on_stdin_eof).await;
            shutdown.cancel();
            running.begin_shutdown().await;
        })
        .await?;
    let result = runtime.shutdown().await;
    drop(access);
    result.map_err(Into::into)
}

async fn wait_for_shutdown(exit_on_stdin_eof: bool) {
    if !exit_on_stdin_eof {
        wait_for_signal().await;
        return;
    }

    // The desktop holds the write end until it exits, including on a crash.
    let (closed, wait) = tokio::sync::oneshot::channel();
    // Named so a thread dump during a hung shutdown points at this wait. If the
    // OS refuses the thread, the host falls back to signal shutdown rather than
    // hanging forever on a wait nothing will satisfy.
    let spawned = std::thread::Builder::new()
        .name("ene-host-stdin-eof".into())
        .spawn(move || {
            // Draining stdin blocks until the desktop closes the pipe; the byte
            // count and any I/O error are both expected to be uninteresting.
            let _drained = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            // The receiver stops listening once the host is already closing.
            let _signalled = closed.send(());
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot watch stdin; falling back to signal shutdown");
        wait_for_signal().await;
        return;
    }
    // This wait ends by design when the pipe closes.
    let _closed = wait.await;
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
    // A signal handler that cannot be installed leaves the host running
    // until the pipe closes, which is the non-unix fallback anyway.
    let _signal = tokio::signal::ctrl_c().await;
}
