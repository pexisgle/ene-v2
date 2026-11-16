//! The one HTTP path a plugin has.
//!
//! A plugin never holds a credential and never chooses a destination the host
//! did not authorize. It asks for an exchange, the host decides whether that
//! destination is allowed for the provider the plugin serves, attaches the
//! credential itself, and reads the reply back in bounded chunks.
//!
//! Every transport guard lives here rather than in each provider: redirects
//! stay off, one exchange has a deadline, a reply body is capped, and a plugin
//! cannot override the headers the host sets.
use crate::limits::{HTTP_TIMEOUT, MAX_HTTP_BODY, MAX_HTTP_CHUNK, MAX_HTTP_HEADERS};
use ene_plugin::{Failure, HttpChunk, HttpRequest};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use url::Url;

/// What the host lets one provider do over HTTP.
///
/// The host implements this; a plugin cannot. `provider` is the provider the
/// plugin was loaded for, so a plugin serving one provider cannot borrow
/// another's destinations or credential.
pub trait HttpGrants: Send + Sync {
    /// Whether `provider` may send to `url`. The host compares origins against
    /// the destinations its manifest declared.
    fn allows(&self, provider: &str, url: &Url) -> bool;
    /// The headers `provider` sends with. The host attaches its credential
    /// here, so the value never reaches the plugin.
    fn authorize(&self, provider: &str, headers: &mut HeaderMap) -> Result<(), Failure>;
}

/// Refuses every exchange. The default for a host that has not opted in.
pub struct DenyHttp;
impl HttpGrants for DenyHttp {
    fn allows(&self, _: &str, _: &Url) -> bool {
        false
    }
    fn authorize(&self, _: &str, _: &mut HeaderMap) -> Result<(), Failure> {
        Err(Failure::new("http_denied"))
    }
}

/// One open reply, read back a chunk at a time.
struct Open {
    status: u16,
    headers: BTreeMap<String, String>,
    /// Buffered under the body cap and exchange timeout before open returns.
    /// Handles live until read, close, or broker drop; the caller owns that scope.
    body: Vec<u8>,
    /// How much of `body` has been handed out.
    read: usize,
}

/// The process's HTTP path for plugins.
pub struct HttpBroker {
    grants: Arc<dyn HttpGrants>,
    client: reqwest::Client,
    open: Mutex<HashMap<String, Open>>,
    next: AtomicU64,
}

impl HttpBroker {
    /// A broker that authorizes through `grants`. Redirects stay off so a
    /// checked destination cannot bounce the request elsewhere.
    pub fn new(grants: Arc<dyn HttpGrants>) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            grants,
            client,
            open: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// Buffers the bounded reply under one exchange timeout, then returns a handle.
    /// Reads split that buffer into ABI-sized chunks, not live network updates.
    pub async fn open(&self, provider: &str, request: HttpRequest) -> Result<HttpChunk, Failure> {
        let url = Url::parse(&request.url).map_err(|_| Failure::new("invalid_url"))?;
        if !self.grants.allows(provider, &url) {
            return Err(Failure::new("destination_denied"));
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| Failure::new("invalid_method"))?;
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            // A plugin may add headers, but not the ones the host owns: the
            // credential is the host's to attach, and a plugin that sets it
            // would be choosing what the host authenticates as.
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Failure::new("invalid_header"))?;
            if is_host_owned(&name) {
                return Err(Failure::new("reserved_header"));
            }
            let value = HeaderValue::from_str(value).map_err(|_| Failure::new("invalid_header"))?;
            headers.insert(name, value);
        }
        self.grants.authorize(provider, &mut headers)?;
        let body = request
            .body_base64
            .map(|encoded| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| Failure::new("invalid_body"))
            })
            .transpose()?;
        let mut call = self.client.request(method, url).headers(headers);
        if let Some(body) = body {
            call = call.body(body);
        }
        let (status, headers, body) = tokio::time::timeout(HTTP_TIMEOUT, async {
            let response = call.send().await.map_err(|error| Failure {
                code: "http_failed".into(),
                message: error.to_string(),
            })?;
            let status = response.status().as_u16();
            let headers = copy_headers(response.headers());
            let body = bounded(response).await?;
            Ok::<_, Failure>((status, headers, body))
        })
        .await
        .map_err(|_| Failure::new("http_timeout"))??;
        let handle = format!("http-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut open = self
            .open
            .lock()
            .map_err(|_| Failure::new("http_unavailable"))?;
        open.insert(
            handle.clone(),
            Open {
                status,
                headers,
                body,
                read: 0,
            },
        );
        Ok(HttpChunk {
            handle,
            status,
            headers: BTreeMap::new(),
            data_base64: String::new(),
            done: false,
        })
    }

    /// Hands out the next chunk of `handle`.
    pub fn read(&self, handle: &str) -> Result<HttpChunk, Failure> {
        use base64::Engine;

        let mut open = self
            .open
            .lock()
            .map_err(|_| Failure::new("http_unavailable"))?;
        let entry = open
            .get_mut(handle)
            .ok_or_else(|| Failure::new("unknown_handle"))?;
        let take = (entry.body.len() - entry.read).min(MAX_HTTP_CHUNK);
        let start = entry.read;
        entry.read += take;
        let done = entry.read >= entry.body.len();
        // The reply headers ride the first chunk only.
        let headers = if start == 0 {
            std::mem::take(&mut entry.headers)
        } else {
            BTreeMap::new()
        };
        let status = entry.status;
        let chunk =
            base64::engine::general_purpose::STANDARD.encode(&entry.body[start..entry.read]);
        if done {
            open.remove(handle);
        }
        Ok(HttpChunk {
            handle: handle.to_owned(),
            status,
            headers,
            data_base64: chunk,
            done,
        })
    }

    /// Drops `handle` without reading the rest. Closing a handle whose body was
    /// already read to its end succeeds: the end state is the same, so a guest
    /// may close unconditionally.
    pub fn close(&self, handle: &str) -> Result<(), Failure> {
        let mut open = self
            .open
            .lock()
            .map_err(|_| Failure::new("http_unavailable"))?;
        open.remove(handle);
        Ok(())
    }
}

/// Headers the host owns. A plugin that sets one is refused rather than
/// silently overridden, so a provider author sees the refusal.
fn is_host_owned(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "host" | "content-length"
    )
}

/// The reply headers a plugin may see, bounded.
fn copy_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut copied = BTreeMap::new();
    let mut total = 0usize;
    for (name, value) in headers {
        // The host's own credential and transport details stay out of the
        // plugin's view.
        if is_host_owned(name) || matches!(name.as_str(), "set-cookie" | "transfer-encoding") {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        total += name.as_str().len() + value.len();
        if total > MAX_HTTP_HEADERS {
            break;
        }
        copied.insert(name.as_str().to_owned(), value.to_owned());
    }
    copied
}

/// Reads a reply body, refusing one larger than the cap.
async fn bounded(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    use http_body_util::BodyExt;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY as u64)
    {
        return Err(Failure::new("http_body_too_large"));
    }
    let limited = http_body_util::Limited::new(reqwest::Body::from(response), MAX_HTTP_BODY);
    limited
        .collect()
        .await
        .map(|body| body.to_bytes().to_vec())
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                Failure::new("http_body_too_large")
            } else {
                Failure {
                    code: "http_failed".into(),
                    message: error.to_string(),
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grants that allow one origin and attach one bearer.
    struct Allow(String);
    impl HttpGrants for Allow {
        fn allows(&self, _: &str, url: &Url) -> bool {
            url.origin().ascii_serialization() == self.0
        }
        fn authorize(&self, _: &str, headers: &mut HeaderMap) -> Result<(), Failure> {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer host-secret"),
            );
            Ok(())
        }
    }

    /// One server that answers every request with `body`.
    async fn serve(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        (format!("http://{address}"), task)
    }

    fn request(url: String) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url,
            headers: BTreeMap::new(),
            body_base64: None,
        }
    }

    #[tokio::test]
    async fn a_destination_the_grant_refuses_never_reaches_the_network() {
        let broker = HttpBroker::new(Arc::new(Allow("http://allowed.test".into()))).unwrap();
        let error = broker
            .open("provider", request("http://denied.test/x".into()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "destination_denied");
    }

    #[tokio::test]
    async fn the_host_attaches_the_credential_and_the_plugin_never_sees_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
            String::from_utf8_lossy(&head).into_owned()
        });
        let broker = HttpBroker::new(Arc::new(Allow(format!("http://{address}")))).unwrap();
        broker
            .open("provider", request(format!("http://{address}/x")))
            .await
            .unwrap();
        let head = seen.await.unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer host-secret"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn a_plugin_cannot_set_a_header_the_host_owns() {
        let broker = HttpBroker::new(Arc::new(Allow("http://allowed.test".into()))).unwrap();
        let mut headers = BTreeMap::new();
        headers.insert("AuThOrIzAtIoN".into(), "Bearer plugin-chosen".into());
        let mut call = request("http://allowed.test/x".into());
        call.headers = headers;
        let error = broker.open("provider", call).await.unwrap_err();
        assert_eq!(error.code, "reserved_header");
    }

    #[tokio::test]
    async fn a_reply_is_read_back_in_bounded_chunks() {
        use base64::Engine;

        let body: &'static [u8] = b"0123456789";
        let (url, server) = serve(body).await;
        let broker = HttpBroker::new(Arc::new(Allow(url.clone()))).unwrap();
        let first = broker
            .open("provider", request(format!("{url}/x")))
            .await
            .unwrap();
        assert!(!first.done, "the opening chunk carries no body");
        let mut read = Vec::new();
        let mut chunk = broker.read(&first.handle).unwrap();
        loop {
            read.extend_from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data_base64)
                    .unwrap(),
            );
            if chunk.done {
                break;
            }
            chunk = broker.read(&first.handle).unwrap();
        }
        assert_eq!(read, body);
        // The handle is spent once its body is handed out.
        assert_eq!(
            broker.read(&first.handle).unwrap_err().code,
            "unknown_handle"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_reply_larger_than_the_cap_is_refused() {
        let big: &'static [u8] = Box::leak(vec![b'x'; MAX_HTTP_BODY + 1].into_boxed_slice());
        let (url, server) = serve(big).await;
        let broker = HttpBroker::new(Arc::new(Allow(url.clone()))).unwrap();
        let error = broker
            .open("provider", request(format!("{url}/x")))
            .await
            .unwrap_err();
        assert_eq!(error.code, "http_body_too_large");
        server.abort();
    }

    #[tokio::test]
    async fn closing_a_handle_drops_it_without_reading_the_rest() {
        let (url, server) = serve(b"unread").await;
        let broker = HttpBroker::new(Arc::new(Allow(url.clone()))).unwrap();
        let opened = broker
            .open("provider", request(format!("{url}/x")))
            .await
            .unwrap();
        broker.close(&opened.handle).unwrap();
        assert_eq!(
            broker.read(&opened.handle).unwrap_err().code,
            "unknown_handle"
        );
        server.abort();
    }

    #[tokio::test]
    async fn redirects_never_reach_an_unchecked_destination() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let forbidden = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = forbidden.local_addr().unwrap();
        let allowed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", allowed.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = allowed.accept().await.unwrap();
            let mut request = [0; 4096];
            assert_ne!(socket.read(&mut request).await.unwrap(), 0);
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target}/secret\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        });
        let broker = HttpBroker::new(Arc::new(Allow(origin.clone()))).unwrap();
        let opened = broker.open("provider", request(origin)).await.unwrap();
        assert_eq!(opened.status, 302);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), forbidden.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn prompt_headers_do_not_leave_body_reads_unbounded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert_ne!(socket.read(&mut request).await.unwrap(), 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let broker = HttpBroker::new(Arc::new(Allow(origin.clone()))).unwrap();
        let result = tokio::time::timeout(
            HTTP_TIMEOUT + std::time::Duration::from_secs(2),
            broker.open("provider", request(origin)),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap().unwrap_err().code, "http_timeout");
        assert!(broker.open.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_length_bodies_are_also_capped() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert_ne!(socket.read(&mut request).await.unwrap(), 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let _ = socket.write_all(&vec![b'x'; MAX_HTTP_BODY + 1]).await;
        });
        let broker = HttpBroker::new(Arc::new(Allow(origin.clone()))).unwrap();
        assert_eq!(
            broker
                .open("provider", request(origin))
                .await
                .unwrap_err()
                .code,
            "http_body_too_large"
        );
        assert!(broker.open.lock().unwrap().is_empty());
        server.await.unwrap();
    }

    #[test]
    fn protected_response_headers_are_never_copied() {
        let mut headers = HeaderMap::new();
        for name in [
            "authorization",
            "proxy-authorization",
            "cookie",
            "set-cookie",
            "host",
            "content-length",
            "transfer-encoding",
        ] {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static("host-secret"),
            );
        }
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        assert_eq!(
            copy_headers(&headers),
            BTreeMap::from([("content-type".into(), "application/json".into())])
        );
    }

    #[tokio::test]
    async fn the_denying_broker_refuses_every_exchange() {
        let broker = HttpBroker::new(Arc::new(DenyHttp)).unwrap();
        let error = broker
            .open("provider", request("http://127.0.0.1:1/x".into()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "destination_denied");
    }
}
