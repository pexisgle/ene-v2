//! The host's HTTP clients.
//!
//! One client per *behaviour*, not per timeout: [`client`] is the pool every
//! request path shares, with each caller setting its own deadline on the
//! request (`RequestBuilder::timeout`), and [`model_client`] is the one rig
//! builds model requests on, where the deadline can only sit on the client.
//! They stay apart because the same [`client`] also carries an MCP stream,
//! which is long-lived by design.
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

pub(crate) async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("the {label} response failed: {}", error.without_url()))?
    {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("the {label} response exceeded the host limit"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
