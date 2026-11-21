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

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum NetError {
    #[error("http client: {0}")]
    ClientInit(String),
    #[error("the {label} response failed: {error}")]
    ResponseRead { label: String, error: String },
    #[error("the {0} response exceeded the host limit")]
    ResponseTooLarge(String),
}

fn build(timeout: Option<Duration>) -> Result<reqwest::Client, NetError> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .map_err(|error| NetError::ClientInit(error.to_string()))
}

/// The process-wide client for every request path but model calls; clones
/// share the connection pool. Redirects stay off so a checked endpoint cannot
/// bounce the request elsewhere. It sets no timeout of its own: callers that
/// know their budget bound the request, and an MCP stream is not bounded.
pub(crate) fn client() -> Result<reqwest::Client, NetError> {
    static CLIENT: OnceLock<Result<reqwest::Client, NetError>> = OnceLock::new();
    CLIENT.get_or_init(|| build(None)).clone()
}

/// The client rig's model transports send through. Rig builds the requests,
/// so the deadline has to be the client's; keeping it separate leaves
/// [`client`]'s long-lived MCP stream unbounded.
pub(crate) fn model_client() -> Result<reqwest::Client, NetError> {
    static CLIENT: OnceLock<Result<reqwest::Client, NetError>> = OnceLock::new();
    CLIENT.get_or_init(|| build(Some(MODEL_TIMEOUT))).clone()
}

pub(crate) async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| NetError::ResponseRead {
            label: label.to_owned(),
            error: error.without_url().to_string(),
        })?
    {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(NetError::ResponseTooLarge(label.to_owned()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
