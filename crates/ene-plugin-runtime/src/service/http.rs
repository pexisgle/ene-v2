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
mod headers;

use crate::limits::{HTTP_TIMEOUT, MAX_HTTP_BODY, MAX_HTTP_CHUNK};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use ene_plugin::{Failure, HttpChunk, HttpRequest};
use futures_util::StreamExt as _;
use headers::{copy_headers, request_headers};
use http_body_util::{BodyDataStream, BodyExt, Limited};
use parking_lot::Mutex;
use reqwest::header::HeaderMap;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::time::{Instant, timeout_at};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
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
#[derive(Debug)]
pub struct DenyHttp;
impl HttpGrants for DenyHttp {
    fn allows(&self, _: &str, _: &Url) -> bool {
        false
    }
    fn authorize(&self, _: &str, _: &mut HeaderMap) -> Result<(), Failure> {
        Err(Failure::new("http_denied"))
    }
}

/// One reply. Buffered replies hold all their bytes; streamed replies retain
/// at most one network chunk alongside the connection.
struct Open {
    status: u16,
    headers: BTreeMap<String, String>,
    pending: Bytes,
    stream: Option<Streaming>,
}

struct Streaming {
    body: mpsc::Receiver<Bytes>,
    transport: AbortOnDropHandle<Result<(), Failure>>,
    deadline: Instant,
}

struct Reply {
    stop: CancellationToken,
    body: AsyncMutex<Open>,
}

/// The process's HTTP path for plugins.
pub struct HttpBroker {
    grants: Arc<dyn HttpGrants>,
    client: reqwest::Client,
    open: Mutex<HashMap<String, Arc<Reply>>>,
    next: Arc<AtomicU64>,
    lifetime: CancellationToken,
    /// How long one exchange may take, end to end.
    deadline: Duration,
}

impl std::fmt::Debug for HttpBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpBroker")
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl HttpBroker {
    /// A broker that authorizes through `grants`. Redirects stay off so a
    /// checked destination cannot bounce the request elsewhere.
    pub fn new(grants: Arc<dyn HttpGrants>) -> Result<Self, reqwest::Error> {
        Self::with_deadline(grants, HTTP_TIMEOUT)
    }

    /// [`Self::new`] with a whole-exchange budget of `deadline`. A streamed
    /// generation may legitimately outlast the short transport budget, while
    /// every exchange still has a bound.
    pub fn with_deadline(
        grants: Arc<dyn HttpGrants>,
        deadline: Duration,
    ) -> Result<Self, reqwest::Error> {
        Self::with_lifetime(grants, Some(deadline), CancellationToken::new())
    }

    /// Creates a broker whose exchanges end when its admission is revoked.
    pub fn with_lifetime(
        grants: Arc<dyn HttpGrants>,
        deadline: Option<Duration>,
        lifetime: CancellationToken,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            grants,
            client,
            open: Mutex::new(HashMap::new()),
            next: Arc::new(AtomicU64::new(1)),
            lifetime,
            deadline: deadline.unwrap_or(HTTP_TIMEOUT),
        })
    }

    /// Shares the client and grants, with replies private to this invocation.
    pub fn for_invocation(&self) -> Self {
        Self {
            grants: Arc::clone(&self.grants),
            client: self.client.clone(),
            open: Mutex::default(),
            next: Arc::clone(&self.next),
            lifetime: self.lifetime.child_token(),
            deadline: self.deadline,
        }
    }

    /// Rejects work and drops retained replies after the admission ends.
    pub fn check(&self) -> Result<(), Failure> {
        if self.lifetime.is_cancelled() {
            self.open.lock().clear();
            return Err(Failure::new("provider_revoked"));
        }
        Ok(())
    }

    async fn while_live<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, Failure>>,
    ) -> Result<T, Failure> {
        let result = tokio::select! {
            biased;
            () = self.lifetime.cancelled() => Err(Failure::new("provider_revoked")),
            result = work => result,
        };
        self.check()?;
        result
    }

    /// Opens a bounded reply. A buffered request reads the whole body under
    /// the exchange deadline before returning; a streaming request returns as
    /// soon as the headers arrive and hands out chunks as the network delivers
    /// them, under the same deadline and body cap.
    pub async fn open(&self, provider: &str, request: HttpRequest) -> Result<HttpChunk, Failure> {
        self.while_live(self.open_reply(provider, request)).await
    }

    async fn open_reply(&self, provider: &str, request: HttpRequest) -> Result<HttpChunk, Failure> {
        let url = Url::parse(&request.url).map_err(|_| Failure::new("invalid_url"))?;
        if !self.grants.allows(provider, &url) {
            return Err(Failure::new("destination_denied"));
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| Failure::new("invalid_method"))?;
        let mut headers = request_headers(&request.headers)?;
        self.grants.authorize(provider, &mut headers)?;
        let mut call = self.client.request(method, url).headers(headers);
        if let Some(encoded) = request.body_base64 {
            call = call.body(
                STANDARD
                    .decode(encoded)
                    .map_err(|_| Failure::new("invalid_body"))?,
            );
        }
        self.check()?;
        let deadline = Instant::now() + self.deadline;
        let handle = format!("http-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let response = timeout_at(deadline, call.send())
            .await
            .map_err(|_| Failure::new("http_timeout"))?
            .map_err(|error| body_error(&error))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_HTTP_BODY as u64)
        {
            return Err(Failure::new("http_body_too_large"));
        }
        let status = response.status().as_u16();
        let mut entry = Open {
            status,
            headers: copy_headers(response.headers()),
            pending: Bytes::new(),
            stream: None,
        };
        let body = Limited::new(reqwest::Body::from(response), MAX_HTTP_BODY);
        let stop = self.lifetime.child_token();
        let headers = if request.stream {
            entry.stream = Some(stream_reply(
                body.into_data_stream(),
                deadline,
                stop.clone(),
            ));
            std::mem::take(&mut entry.headers)
        } else {
            entry.pending = timeout_at(deadline, body.collect())
                .await
                .map_err(|_| Failure::new("http_timeout"))?
                .map_err(|error| body_error(error.as_ref()))?
                .to_bytes();
            BTreeMap::new()
        };
        self.open.lock().insert(
            handle.clone(),
            Arc::new(Reply {
                stop,
                body: AsyncMutex::new(entry),
            }),
        );
        Ok(HttpChunk {
            handle,
            status,
            headers,
            data_base64: String::new(),
            done: false,
        })
    }

    /// Hands out the next chunk of `handle`. A streaming read awaits the next
    /// network chunk under the exchange deadline; dropping the read future
    /// drops the response and aborts the exchange.
    pub async fn read(&self, handle: &str) -> Result<HttpChunk, Failure> {
        self.while_live(self.read_reply(handle)).await
    }

    async fn read_reply(&self, handle: &str) -> Result<HttpChunk, Failure> {
        let reply = self
            .open
            .lock()
            .get(handle)
            .cloned()
            .ok_or_else(|| Failure::new("unknown_handle"))?;
        let mut entry = reply
            .body
            .try_lock()
            .map_err(|_| Failure::new("unknown_handle"))?;
        let on_drop = reply.stop.drop_guard_ref();
        let result = tokio::select! {
            biased;
            () = reply.stop.cancelled() => Err(Failure::new("unknown_handle")),
            result = entry.next_chunk(handle) => result,
        };
        let result = if on_drop.disarm().is_cancelled() {
            Err(Failure::new("unknown_handle"))
        } else {
            result
        };
        if result.as_ref().map_or(true, |chunk| chunk.done) {
            self.open.lock().remove(handle);
        }
        result
    }

    /// Drops the reply and interrupts an in-flight read. Closing an already
    /// closed or fully read response remains successful.
    pub fn close(&self, handle: &str) -> Result<(), Failure> {
        if let Some(reply) = self.open.lock().remove(handle) {
            reply.stop.cancel();
        }
        Ok(())
    }
}

impl Open {
    async fn next_chunk(&mut self, handle: &str) -> Result<HttpChunk, Failure> {
        if let Some(stream) = &mut self.stream {
            if Instant::now() >= stream.deadline {
                return Err(Failure::new("http_timeout"));
            }
            if self.pending.is_empty() {
                if let Some(bytes) = stream.body.recv().await {
                    if Instant::now() >= stream.deadline {
                        return Err(Failure::new("http_timeout"));
                    }
                    self.pending = bytes;
                } else {
                    (&mut stream.transport)
                        .await
                        .map_err(|_| Failure::new("http_failed"))??;
                    self.stream = None;
                }
            }
        }
        let bytes = self
            .pending
            .split_to(self.pending.len().min(MAX_HTTP_CHUNK));
        Ok(HttpChunk {
            handle: handle.to_owned(),
            status: self.status,
            headers: std::mem::take(&mut self.headers),
            data_base64: STANDARD.encode(bytes),
            done: self.stream.is_none() && self.pending.is_empty(),
        })
    }
}

fn stream_reply(
    mut body: BodyDataStream<Limited<reqwest::Body>>,
    deadline: Instant,
    stop: CancellationToken,
) -> Streaming {
    let (send, receive) = mpsc::channel(1);
    let transport = AbortOnDropHandle::new(tokio::spawn(async move {
        let transfer = async {
            while let Some(bytes) = body
                .next()
                .await
                .transpose()
                .map_err(|error| body_error(error.as_ref()))?
            {
                if send.send(bytes).await.is_err() {
                    break;
                }
            }
            Ok(())
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => Err(Failure::new("unknown_handle")),
            result = timeout_at(deadline, transfer) => result.map_err(|_| Failure::new("http_timeout"))?,
        }
    }));
    Streaming {
        body: receive,
        transport,
        deadline,
    }
}

fn body_error(error: &(dyn std::error::Error + 'static)) -> Failure {
    if error.is::<http_body_util::LengthLimitError>() {
        Failure::new("http_body_too_large")
    } else {
        Failure {
            code: "http_failed".into(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

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
                    if let Err(error) = stream.write_all(reply.as_bytes()).await {
                        tracing::debug!(%error, "http reply write failed");
                    }
                    if let Err(error) = stream.write_all(body).await {
                        tracing::debug!(%error, "http body write failed");
                    }
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
            stream: false,
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
            if let Err(error) = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
            {
                tracing::debug!(%error, "test reply write failed");
            }
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
        static BODY: [u8; 3 * MAX_HTTP_CHUNK + 17] = [b'x'; 3 * MAX_HTTP_CHUNK + 17];
        let (url, server) = serve(&BODY).await;
        let broker = HttpBroker::new(Arc::new(Allow(url.clone()))).unwrap();
        for stream in [false, true] {
            let mut call = request(format!("{url}/x"));
            call.stream = stream;
            let first = broker.open("provider", call).await.unwrap();
            assert!(!first.done && first.data_base64.is_empty());
            assert_eq!(first.headers.contains_key("content-type"), stream);
            let mut read = Vec::new();
            loop {
                let chunk = broker.read(&first.handle).await.unwrap();
                assert_eq!(chunk.status, 200);
                assert_eq!(
                    chunk.headers.contains_key("content-type"),
                    !stream && read.is_empty()
                );
                let bytes = STANDARD.decode(&chunk.data_base64).unwrap();
                assert!(bytes.len() <= MAX_HTTP_CHUNK);
                read.extend(bytes);
                if chunk.done {
                    break;
                }
            }
            assert_eq!(read, BODY);
            assert_eq!(
                broker.read(&first.handle).await.unwrap_err().code,
                "unknown_handle"
            );
        }
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
            broker.read(&opened.handle).await.unwrap_err().code,
            "unknown_handle"
        );
        server.abort();
    }

    async fn held_reply() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut tail = [0; 4];
            while &tail != b"\r\n\r\n" {
                tail.rotate_left(1);
                tail[3] = socket.read_u8().await.unwrap();
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
        });
        (origin, peer)
    }

    #[tokio::test]
    async fn a_stream_deadline_closes_the_peer_without_a_reader() {
        let (origin, peer) = held_reply().await;
        let broker =
            HttpBroker::with_deadline(Arc::new(Allow(origin.clone())), Duration::from_millis(300))
                .unwrap();
        let mut call = request(origin);
        call.stream = true;
        let reply = broker.open("provider", call).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            broker.read(&reply.handle).await.unwrap_err().code,
            "http_timeout"
        );
        assert!(broker.open.lock().is_empty());
    }

    #[tokio::test]
    async fn closing_or_abandoning_a_read_retires_the_reply() {
        for close in [true, false] {
            let (origin, peer) = held_reply().await;
            let broker = HttpBroker::new(Arc::new(Allow(origin.clone()))).unwrap();
            let mut call = request(origin);
            call.stream = true;
            let reply = broker.open("provider", call).await.unwrap();
            let mut read = Box::pin(broker.read(&reply.handle));
            assert!(futures_util::poll!(&mut read).is_pending());
            if close {
                broker.close(&reply.handle).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(3), read)
                    .await
                    .unwrap();
                assert_eq!(result.unwrap_err().code, "unknown_handle");
            } else {
                drop(read);
            }
            tokio::time::timeout(Duration::from_secs(3), peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                broker.read(&reply.handle).await.unwrap_err().code,
                "unknown_handle"
            );
            assert!(broker.open.lock().is_empty());
        }
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
        assert!(broker.open.lock().is_empty());
    }

    /// A caller that streams a long generation sets its own whole-exchange
    /// budget; the bound stays, only its length changes.
    #[tokio::test]
    async fn revocation_ends_a_stream_waiting_for_the_upstream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let _read = connection.read(&mut [0; 1024]).await.unwrap();
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let lifetime = CancellationToken::new();
        let broker =
            HttpBroker::with_lifetime(Arc::new(Allow(origin.clone())), None, lifetime.clone())
                .unwrap();
        let mut request = request(origin);
        request.stream = true;
        let reply = broker.open("provider", request).await.unwrap();
        let read = broker.read(&reply.handle);
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
        lifetime.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), read)
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().code, "provider_revoked");
        server.abort();
    }

    #[tokio::test]
    async fn a_caller_can_set_the_exchange_budget() {
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
        let broker =
            HttpBroker::with_deadline(Arc::new(Allow(origin.clone())), Duration::from_millis(100))
                .unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            broker.open("provider", request(origin)),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap().unwrap_err().code, "http_timeout");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn unknown_length_bodies_are_also_capped() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let oversized = vec![b'x'; MAX_HTTP_BODY + 1];
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert_ne!(socket.read(&mut request).await.unwrap(), 0);
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                if let Err(error) = socket.write_all(&oversized).await {
                    tracing::debug!(%error, "oversized fixture write failed");
                }
            }
        });
        let broker = HttpBroker::new(Arc::new(Allow(origin.clone()))).unwrap();
        assert_eq!(
            broker
                .open("provider", request(origin.clone()))
                .await
                .unwrap_err()
                .code,
            "http_body_too_large"
        );
        let mut call = request(origin);
        call.stream = true;
        let opened = broker.open("provider", call).await.unwrap();
        loop {
            match broker.read(&opened.handle).await {
                Ok(chunk) => assert!(!chunk.done),
                Err(error) => {
                    assert_eq!(error.code, "http_body_too_large");
                    break;
                }
            }
        }
        assert!(broker.open.lock().is_empty());
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
