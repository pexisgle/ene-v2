//! The host's HTTP clients, and the bounded body read the voice engines,
//! embeddings, the model probe and the MCP transports share.
//!
//! One client per *behaviour*, not per timeout: [`client`] is the pool every
//! request path shares, with each caller setting its own deadline on the
//! request (`RequestBuilder::timeout`), and [`model_client`] is the one rig
//! builds model requests on, where the deadline can only sit on the client.
//! They stay apart because the same [`client`] also carries an MCP stream,
//! which is long-lived by design.
use futures_util::StreamExt;
use std::{sync::OnceLock, time::Duration};

/// How long one model call may take end to end.
const MODEL_TIMEOUT: Duration = Duration::from_secs(120);

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
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("the {what} endpoint body: {error}"))?;
        if chunk.len() > max - body.len() {
            return Err(too_large(what, max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large(what: &str, max: usize) -> String {
    format!("the {what} endpoint answered more data than the host reads ({max} bytes)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one fixed HTTP response and returns its address.
    async fn serve(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 1_024];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(&response).await;
            }
        });
        address
    }

    #[tokio::test]
    async fn bodies_are_refused_at_the_cap_declared_or_streamed() {
        let mut chunked = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        chunked.extend_from_slice(&[b'x'; 128]);
        let declared = b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\n\r\n".to_vec();
        for response in [declared, chunked] {
            let address = serve(response).await;
            let response = client()
                .unwrap()
                .get(format!("http://{address}/"))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .unwrap();
            let error = read_body(response, 64, "test").await.unwrap_err();
            assert!(error.contains("more data"), "{error}");
        }
    }
}
