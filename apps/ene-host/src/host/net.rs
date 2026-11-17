//! The host's HTTP clients, and the bounded body read the voice engines,
//! embeddings, the model probe and the MCP transports share.
//!
//! One client per *behaviour*, not per timeout: [`client`] is the pool every
//! request path shares, with each caller setting its own deadline on the
//! request (`RequestBuilder::timeout`), and [`model_client`] is the one rig
//! builds model requests on, where the deadline can only sit on the client.
//! They stay apart because the same [`client`] also carries an MCP stream,
//! which is long-lived by design.
use http_body_util::{BodyExt, LengthLimitError, Limited};
use std::{sync::OnceLock, time::Duration};

/// How long one model call may take end to end.
pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(120);

fn build(timeout: Option<Duration>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .map_err(|error| format!("http client: {error}"))
}

/// The process-wide client for every request path but model calls; clones
/// share the connection pool. Redirects stay off so a checked endpoint cannot
/// bounce the request elsewhere. It sets no timeout of its own: callers that
/// know their budget bound the request, and an MCP stream is not bounded.
pub(crate) fn client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build(None)).clone()
}

/// The client rig's model transports send through. Rig builds the requests,
/// so the deadline has to be the client's; keeping it separate leaves
/// [`client`]'s long-lived MCP stream unbounded.
pub(crate) fn model_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build(Some(MODEL_TIMEOUT))).clone()
}

pub(crate) async fn read_body(
    response: reqwest::Response,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(too_large(what, max));
    }
    Limited::new(reqwest::Body::from(response), max)
        .collect()
        .await
        .map(|body| body.to_bytes().to_vec())
        .map_err(|error| {
            if error.is::<LengthLimitError>() {
                too_large(what, max)
            } else {
                format!("the {what} endpoint body: {error}")
            }
        })
}

fn too_large(what: &str, max: usize) -> String {
    format!("the {what} endpoint answered more data than the host reads ({max} bytes)")
}
