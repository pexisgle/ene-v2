//! The shared HTTP shape: one pooled client per timeout, and the bounded body
//! read the voice engines, embeddings and the model probe share.
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

/// A client per timeout; clones share the connection pool. Redirects stay off
/// so a checked endpoint cannot bounce the request elsewhere.
pub(crate) fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    static CLIENTS: OnceLock<Mutex<HashMap<u64, reqwest::Client>>> = OnceLock::new();
    let millis = u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let clients = CLIENTS.get_or_init(Default::default);
    let guard = || crate::host::lock(clients);
    if let Some(client) = guard().get(&millis).cloned() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(millis))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    guard().entry(millis).or_insert(client.clone());
    Ok(client)
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
            let response = client(Duration::from_secs(5))
                .unwrap()
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            let error = read_body(response, 64, "test").await.unwrap_err();
            assert!(error.contains("more data"), "{error}");
        }
    }
}
