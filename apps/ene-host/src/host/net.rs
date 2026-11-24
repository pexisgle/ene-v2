//! The host's HTTP clients.
//!
//! One client per *behaviour*, not per timeout: [`client`] is the pool every
//! request path shares, with each caller setting its own deadline on the
//! request (`RequestBuilder::timeout`), and [`model_client`] is the one rig
//! builds model requests on, where the deadline can only sit on the client.
//! They stay apart because the same [`client`] also carries an MCP stream,
//! which is long-lived by design.
use std::{sync::LazyLock, time::Duration};

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
    static CLIENT: LazyLock<Result<reqwest::Client, NetError>> = LazyLock::new(|| build(None));
    CLIENT.clone()
}

/// The client rig's model transports send through. Rig builds the requests,
/// so the deadline has to be the client's; keeping it separate leaves
/// [`client`]'s long-lived MCP stream unbounded.
pub(crate) fn model_client() -> Result<reqwest::Client, NetError> {
    static CLIENT: LazyLock<Result<reqwest::Client, NetError>> =
        LazyLock::new(|| build(Some(MODEL_TIMEOUT)));
    CLIENT.clone()
}

/// Reads a reply body, refusing one over `max_bytes`. Bounded by
/// `http_body_util::Limited` the way the plugin host's `bounded` is, but the
/// body comes from `bytes_stream` rather than `reqwest::Body::from`: the old
/// `chunk()` loop surfaced read failures as reqwest's decode error, and the
/// byte stream keeps that wrapper, so `ResponseRead` still carries the same
/// message. `Limited` rejects the same bodies the old running total did.
pub(crate) async fn read_response_body(
    response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, NetError> {
    use futures_util::TryStreamExt;
    use http_body_util::BodyExt;
    let body =
        http_body_util::StreamBody::new(response.bytes_stream().map_ok(hyper::body::Frame::data));
    http_body_util::Limited::new(body, max_bytes)
        .collect()
        .await
        .map(|body| body.to_bytes().to_vec())
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                return NetError::ResponseTooLarge(label.to_owned());
            }
            let error = error.downcast::<reqwest::Error>().map_or_else(
                |error| error.to_string(),
                |error| error.without_url().to_string(),
            );
            NetError::ResponseRead {
                label: label.to_owned(),
                error,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `body` once on a loopback port and returns a response for it.
    async fn response_with(body: &'static [u8]) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read the request head first: answering before it is consumed
            // makes hyper report `UnexpectedMessage`.
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.flush().await.unwrap();
            socket.shutdown().await.unwrap();
        });
        client()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_body_at_the_limit_is_read_and_one_byte_over_is_refused() {
        // The old loop summed chunk lengths and refused when the running total
        // exceeded `max_bytes`, so exactly-at-limit must still succeed.
        let body: &'static [u8] = b"0123456789";
        let read = read_response_body(response_with(body).await, body.len(), "test")
            .await
            .expect("a body of exactly the limit must be read");
        assert_eq!(read, body);

        let error = read_response_body(response_with(body).await, body.len() - 1, "test")
            .await
            .expect_err("one byte over the limit must be refused");
        assert!(
            matches!(&error, NetError::ResponseTooLarge(label) if label == "test"),
            "got {error:?}"
        );

        let empty = read_response_body(response_with(b"").await, 0, "test")
            .await
            .expect("an empty body must be readable with a zero limit");
        assert!(empty.is_empty());
    }
}
