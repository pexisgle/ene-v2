//! Task inference consumes Rig's native request; no host-specific chat wire.
use futures_util::future::BoxFuture;
use rig_core::completion::CompletionRequest;
use tokio_util::sync::CancellationToken;

use super::Result;

/// The companion identity is host-owned; the model request is Rig's own type.
pub trait TaskInference: Send + Sync {
    fn complete<'a>(
        &'a self,
        companion_id: String,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String>>;
}
