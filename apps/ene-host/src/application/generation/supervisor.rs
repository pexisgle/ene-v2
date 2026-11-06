//! Shared cancellation, deadlines and panic isolation for external operations.
use super::*;
use futures_util::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe};
use tokio_util::sync::CancellationToken;

pub(in crate::application) async fn execute<T>(
    app: &HostApplication,
    operation: OperationKind,
    cancelled: &CancellationToken,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let mut shutdown = app.shutdown();
    if cancelled.is_cancelled() || *shutdown.borrow() {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(Error::new(ErrorKind::PermissionRequired)),
        _ = shutdown.changed() => Err(Error::new(ErrorKind::PermissionRequired)),
        result = tokio::time::timeout(operation.spec().timeout, AssertUnwindSafe(work).catch_unwind()) => {
            match result {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(Error::new(ErrorKind::ExternalFailure)),
                Err(_) => Err(Error { kind: ErrorKind::Unavailable, reason: Some("upstream_timeout") }),
            }
        }
    }
}
