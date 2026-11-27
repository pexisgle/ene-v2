//! MCP transport helpers (`npx` direct resolution, stdio/HTTP transport opening, and naming).

mod http;
pub mod npx;
mod stdio;
pub mod transport;

pub use npx::is_npx_command;
pub use transport::{ConnectFailure, backoff_delay, exposed_name, open};

async fn until_cancelled<T>(
    stop: &tokio_util::sync::CancellationToken,
    operation: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = stop.cancelled() => None,
        result = operation => Some(result),
    }
}
