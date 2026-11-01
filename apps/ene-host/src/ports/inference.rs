//! Host-mediated task inference for plugin harnesses.
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::Result;

/// One non-streaming task inference request. The host owns the connection,
/// model, credentials and authorization; the guest only supplies the dialogue.
#[derive(Clone, Debug)]
pub struct InferenceRequest {
    pub companion_id: String,
    pub preamble: String,
    /// `(is_user, text)` pairs in conversation order.
    pub messages: Vec<(bool, String)>,
    pub max_tokens: u64,
}

pub trait TaskInference: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: InferenceRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<String>>;
}
