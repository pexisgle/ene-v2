//! The model proxy and loopback relay owned by one isolated ACP launch.

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::{
    body::{Body, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    ffi::OsString,
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinSet;

pub(crate) const RELAY_PORT: u16 = 3128;
pub(crate) const PROXY_VARS: [&str; 2] = ["HTTPS_PROXY", "HTTP_PROXY"];
const MAX_ORIGINS: usize = 32;
const MAX_ADDRESSES: usize = 32;
const MAX_CONNECTIONS: usize = 64;
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bridge {
    pub(crate) socket: PathBuf,
    pub(crate) port: u16,
}

impl Bridge {
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    host: String,
    port: u16,
    http: bool,
}

impl Origin {
    fn parse(endpoint: &str) -> io::Result<Self> {
        if endpoint.len() > 4096 || endpoint.trim() != endpoint {
            return Err(invalid("ACP model endpoint must be an HTTP(S) URL"));
        }
        let url = url::Url::parse(endpoint)
            .map_err(|_| invalid("ACP model endpoint must be an HTTP(S) URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "ACP model endpoint must be HTTP(S), without credentials or a fragment",
            ));
        }
        let host = match url.host() {
            Some(url::Host::Domain(host)) => host.to_owned(),
            Some(url::Host::Ipv4(host)) => host.to_string(),
            Some(url::Host::Ipv6(host)) => host.to_string(),
            None => return Err(invalid("ACP model endpoint needs a host")),
        };
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("ACP model endpoint needs a nonzero port"))?;
        Ok(Self {
            host,
            port,
            http: url.scheme() == "http",
        })
    }

    fn matches(&self, other: &Self) -> bool {
        self.host == other.host && self.port == other.port
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Explicit operator configuration only. Child arguments and environment never grant access.
#[derive(Debug)]
pub(crate) struct ConfiguredOrigins(Vec<Origin>);

impl ConfiguredOrigins {
    pub(crate) fn parse(endpoint: &str, options: &str) -> io::Result<Self> {
        let mut endpoints = Vec::new();
        if !endpoint.trim().is_empty() {
            endpoints.push(endpoint.to_owned());
        }
        if !options.trim().is_empty() {
            let value: serde_json::Value = serde_json::from_str(options)
                .map_err(|_| invalid("ACP connection options must be a JSON object"))?;
            if !value.is_object() {
                return Err(invalid("ACP connection options must be a JSON object"));
            }
            if let Some(extra) = value.get("model_endpoints") {
                let extra = extra.as_array().ok_or_else(|| {
                    invalid("ACP model_endpoints must be an array of HTTP(S) URL strings")
                })?;
                if extra.len() > MAX_ORIGINS {
                    return Err(invalid("too many ACP model_endpoints (maximum 32 origins)"));
                }
                for endpoint in extra {
                    endpoints.push(
                        endpoint
                            .as_str()
                            .ok_or_else(|| {
                                invalid("ACP model_endpoints must contain only HTTP(S) URL strings")
                            })?
                            .to_owned(),
                    );
                }
            }
        }
        let mut origins = Vec::new();
        for endpoint in endpoints {
            let origin = Origin::parse(&endpoint)?;
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        if origins.is_empty() {
            return Err(invalid(
                "isolated ACP needs an approved model endpoint; set the connection endpoint URL or options.model_endpoints to explicit HTTP(S) URLs",
            ));
        }
        if origins.len() > MAX_ORIGINS {
            return Err(invalid("too many ACP model endpoints (maximum 32 origins)"));
        }
        Ok(Self(origins))
    }

    pub(crate) async fn pin(self) -> io::Result<PinnedOrigins> {
        tokio::time::timeout(PREPARE_TIMEOUT, async move {
            let mut pinned: Vec<PinnedOrigin> = Vec::new();
            for origin in self.0 {
                // HTTP and HTTPS on the same authority share one DNS snapshot.
                let addresses = if let Some(previous) = pinned.iter().find(|item| item.origin.matches(&origin)) {
                    previous.addresses.clone()
                } else if let Ok(ip) = origin.host.parse::<IpAddr>() {
                    vec![SocketAddr::new(ip, origin.port)]
                } else {
                    let resolved = tokio::net::lookup_host((origin.host.as_str(), origin.port)).await?;
                    let mut addresses = Vec::new();
                    for address in resolved {
                        if !addresses.contains(&address) {
                            if addresses.len() == MAX_ADDRESSES {
                                return Err(invalid("ACP model endpoint resolved to too many addresses (maximum 32)"));
                            }
                            addresses.push(address);
                        }
                    }
                    addresses
                };
                if addresses.is_empty() {
                    return Err(invalid("ACP model endpoint resolved to no addresses"));
                }
                pinned.push(PinnedOrigin { origin, addresses });
            }
            Ok(PinnedOrigins(pinned))
        }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ACP model endpoint resolution timed out"))?
    }
}

#[derive(Debug)]
struct PinnedOrigin {
    origin: Origin,
    addresses: Vec<SocketAddr>,
}

#[derive(Debug)]
pub(crate) struct PinnedOrigins(Vec<PinnedOrigin>);

impl PinnedOrigins {
    fn approved(&self, origin: &Origin, ordinary_http: bool) -> Option<&PinnedOrigin> {
        self.0
            .iter()
            .find(|item| item.origin.matches(origin) && (!ordinary_http || item.origin.http))
    }
}

impl PinnedOrigin {
    async fn dial(&self) -> io::Result<tokio::net::TcpStream> {
        tokio::time::timeout(DIAL_TIMEOUT, async {
            let mut last =
                io::Error::new(io::ErrorKind::NotConnected, "no pinned ACP model addresses");
            for address in &self.addresses {
                match tokio::net::TcpStream::connect(*address).await {
                    Ok(stream) => return Ok(stream),
                    Err(error) => last = error,
                }
            }
            Err(last)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ACP model connection timed out"))?
    }
}

#[derive(Debug)]
pub(crate) struct Proxy {
    pub(crate) bridge: Bridge,
    handle: tokio::task::JoinHandle<()>,
    closed: AtomicBool,
}

impl Proxy {
    pub(crate) fn close(&self) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            self.handle.abort();
            let _ = std::fs::remove_file(&self.bridge.socket);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(unix)]
pub(crate) fn open(
    runtime: &tokio::runtime::Handle,
    socket: PathBuf,
    policy: PinnedOrigins,
) -> io::Result<Proxy> {
    if socket.as_os_str().len() > 100 {
        return Err(invalid(&format!(
            "the ACP proxy socket path is too long: {} (set TMPDIR to a shorter directory)",
            socket.display()
        )));
    }
    let listener = {
        let _entered = runtime.enter();
        tokio::net::UnixListener::bind(&socket)?
    };
    let policy = Arc::new(policy);
    let handle = runtime.spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { return; };
                    if connections.len() < MAX_CONNECTIONS {
                        let policy = Arc::clone(&policy);
                        connections.spawn(async move { connect(stream, policy).await; });
                    }
                }
            }
        }
    });
    Ok(Proxy {
        bridge: Bridge {
            socket,
            port: RELAY_PORT,
        },
        handle,
        closed: AtomicBool::new(false),
    })
}

#[cfg(not(unix))]
pub(crate) fn open(
    _runtime: &tokio::runtime::Handle,
    _socket: PathBuf,
    _policy: PinnedOrigins,
) -> io::Result<Proxy> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the ACP proxy needs unix sockets",
    ))
}

type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;
type Drivers = Arc<Mutex<JoinSet<()>>>;
type Tunnel = (hyper::upgrade::OnUpgrade, tokio::net::TcpStream);

fn response(status: StatusCode) -> Response<ProxyBody> {
    let mut response = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

fn authority(text: &str, default_scheme: Option<&str>) -> Option<Origin> {
    let parsed = text.parse::<http::uri::Authority>().ok()?;
    if parsed.as_str().contains('@') || parsed.host().is_empty() {
        return None;
    }
    if default_scheme.is_none() && parsed.port_u16().is_none() {
        return None;
    }
    let origin = Origin::parse(&format!("{}://{text}", default_scheme.unwrap_or("https"))).ok()?;
    if text
        .bytes()
        .any(|c| matches!(c, b'/' | b'?' | b'#' | b'\\'))
    {
        return None;
    }
    Some(origin)
}

fn destination(request: &Request<Incoming>) -> Result<(Origin, bool), StatusCode> {
    let connect = request.method() == Method::CONNECT;
    let uri = request.uri();
    let scheme = if connect {
        if uri.scheme().is_some() || uri.path_and_query().is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
        None
    } else {
        if uri.scheme_str() != Some("http") {
            return Err(StatusCode::BAD_REQUEST);
        }
        Some("http")
    };
    let target = uri
        .authority()
        .and_then(|a| authority(a.as_str(), scheme))
        .ok_or(StatusCode::BAD_REQUEST)?;
    let hosts = request.headers().get_all(header::HOST);
    let mut hosts = hosts.iter();
    let host = hosts
        .next()
        .and_then(|h| h.to_str().ok())
        .and_then(|h| authority(h, scheme))
        .ok_or(StatusCode::BAD_REQUEST)?;
    if hosts.next().is_some() || !target.matches(&host) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if connect
        && (request.headers().contains_key(header::CONTENT_LENGTH)
            || request.headers().contains_key(header::TRANSFER_ENCODING)
            || request.headers().contains_key(header::EXPECT))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if request.headers().contains_key(header::UPGRADE) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(encoding) = request.headers().get(header::TRANSFER_ENCODING) {
        if request
            .headers()
            .get_all(header::TRANSFER_ENCODING)
            .iter()
            .count()
            != 1
            || !encoding.as_bytes().eq_ignore_ascii_case(b"chunked")
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    Ok((target, !connect))
}

const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

struct HopHeaders(Vec<header::HeaderName>);

impl HopHeaders {
    fn prepare(headers: &mut HeaderMap) -> Self {
        let filter = Self(Self::names(headers, header::CONNECTION));
        let mut declared = Self::names(headers, header::TRAILER);
        declared.retain(|name| filter.safe_trailer(name));
        filter.strip(headers);
        if !declared.is_empty() {
            let declaration = declared
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            headers.insert(
                header::TRAILER,
                declaration.parse().expect("parsed header names"),
            );
            headers.remove(header::CONTENT_LENGTH);
        }
        filter
    }

    fn names(headers: &HeaderMap, field: header::HeaderName) -> Vec<header::HeaderName> {
        headers
            .get_all(field)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|name| header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
            .collect()
    }

    fn strip(&self, headers: &mut HeaderMap) {
        for name in Self::names(headers, header::CONNECTION)
            .iter()
            .chain(self.0.iter())
        {
            headers.remove(name);
        }
        for name in HOP_HEADERS {
            headers.remove(*name);
        }
    }

    fn safe_trailer(&self, name: &header::HeaderName) -> bool {
        !HOP_HEADERS.contains(&name.as_str())
            && !self.0.contains(name)
            && !matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "authorization"
                    | "content-type"
                    | "content-encoding"
                    | "content-range"
            )
    }

    fn trailers(&self, headers: &mut HeaderMap) {
        self.strip(headers);
        let forbidden: Vec<_> = headers
            .keys()
            .filter(|name| !self.safe_trailer(name))
            .cloned()
            .collect();
        for name in forbidden {
            headers.remove(name);
        }
    }
}

async fn proxy_request(
    mut request: Request<Incoming>,
    policy: Arc<PinnedOrigins>,
    drivers: Drivers,
    tunnel: Arc<Mutex<Option<Tunnel>>>,
) -> Result<Response<ProxyBody>, std::convert::Infallible> {
    let (origin, ordinary) = match destination(&request) {
        Ok(target) => target,
        Err(status) => return Ok(response(status)),
    };
    let Some(approved) = policy.approved(&origin, ordinary) else {
        return Ok(response(StatusCode::FORBIDDEN));
    };
    let stream = match approved.dial().await {
        Ok(stream) => stream,
        Err(_) => return Ok(response(StatusCode::BAD_GATEWAY)),
    };
    if !ordinary {
        *tunnel.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((hyper::upgrade::on(&mut request), stream));
        return Ok(response(StatusCode::OK));
    }
    let (mut sender, connection) = match hyper::client::conn::http1::Builder::new()
        .max_headers(MAX_HEADERS)
        .max_header_size(MAX_HEAD)
        .max_buf_size(MAX_HEAD)
        .handshake::<_, _>(TokioIo::new(stream))
        .await
    {
        Ok(connection) => connection,
        Err(_) => return Ok(response(StatusCode::BAD_GATEWAY)),
    };
    drivers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .spawn(async move {
            let _ = connection.await;
        });
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    *request.uri_mut() = path.parse().expect("a parsed HTTP path");
    let length = request.body().size_hint().exact();
    let has_body = !request.body().is_end_stream();
    let request_hops = HopHeaders::prepare(request.headers_mut());
    request.headers_mut().remove(header::CONTENT_LENGTH);
    if request.headers().contains_key(header::TRAILER) || (has_body && length.is_none()) {
        request.headers_mut().insert(
            header::TRANSFER_ENCODING,
            header::HeaderValue::from_static("chunked"),
        );
    } else if let Some(length) = length {
        request
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length.into());
    }
    let host = if origin.host.contains(':') {
        format!("[{}]:{}", origin.host, origin.port)
    } else {
        format!("{}:{}", origin.host, origin.port)
    };
    request.headers_mut().insert(
        header::HOST,
        host.parse().expect("a canonical HTTP authority"),
    );
    let request = request.map(|body| {
        body.map_frame(move |mut frame| {
            if let Some(trailers) = frame.trailers_mut() {
                request_hops.trailers(trailers);
            }
            frame
        })
    });
    match sender.send_request(request).await {
        Ok(mut upstream) => {
            if upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
                return Ok(response(StatusCode::BAD_GATEWAY));
            }
            let response_hops = HopHeaders::prepare(upstream.headers_mut());
            Ok(upstream.map(|body| {
                body.map_frame(move |mut frame| {
                    if let Some(trailers) = frame.trailers_mut() {
                        response_hops.trailers(trailers);
                    }
                    frame
                })
                .boxed_unsync()
            }))
        }
        Err(_) => Ok(response(StatusCode::BAD_GATEWAY)),
    }
}

#[cfg(unix)]
async fn connect(client: tokio::net::UnixStream, policy: Arc<PinnedOrigins>) {
    let drivers: Drivers = Arc::new(Mutex::new(JoinSet::new()));
    let tunnel = Arc::new(Mutex::new(None));
    let service = service_fn({
        let drivers = Arc::clone(&drivers);
        let tunnel = Arc::clone(&tunnel);
        move |request| {
            proxy_request(
                request,
                Arc::clone(&policy),
                Arc::clone(&drivers),
                Arc::clone(&tunnel),
            )
        }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .keep_alive(false)
        .half_close(true)
        .max_headers(MAX_HEADERS)
        .max_header_size(MAX_HEAD)
        .max_buf_size(MAX_HEAD)
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT)
        .serve_connection(TokioIo::new(client), service)
        .with_upgrades()
        .await;
    drivers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .abort_all();
    let upgraded = tunnel.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((upgrade, mut upstream)) = upgraded {
        if let Ok(Ok(upgraded)) = tokio::time::timeout(HEAD_TIMEOUT, upgrade).await {
            let mut client = TokioIo::new(upgraded);
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    }
}

pub fn relay(args: RelayArgs) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("acp-relay: {error}");
            return 1;
        }
    };
    let listener = match runtime
        .block_on(async { tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await })
    {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("acp-relay: {error}");
            return 1;
        }
    };
    #[cfg(unix)]
    let relay = runtime.spawn(forward(listener, args.socket));
    #[cfg(not(unix))]
    drop(listener);
    let status = std::process::Command::new(&args.command[0])
        .args(&args.command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    #[cfg(unix)]
    {
        relay.abort();
        let _ = runtime.block_on(relay);
    }
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("acp-relay: {error}");
            1
        }
    }
}

#[cfg(unix)]
async fn forward(listener: tokio::net::TcpListener, socket: PathBuf) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else { return; };
                if connections.len() < MAX_CONNECTIONS {
                    let socket = socket.clone();
                    connections.spawn(async move {
                        let Ok(Ok(mut upstream)) = tokio::time::timeout(DIAL_TIMEOUT, tokio::net::UnixStream::connect(&socket)).await else { return; };
                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                    });
                }
            }
        }
    }
}

#[derive(clap::Args, Debug)]
#[command(
    args_override_self = true,
    override_usage = "acp-relay [OPTIONS] --socket <SOCKET> -- <COMMAND>..."
)]
pub struct RelayArgs {
    #[arg(long, allow_hyphen_values = true)]
    socket: PathBuf,
    #[arg(long = "listen", allow_hyphen_values = true, value_name = "ADDR", default_value_t = RELAY_PORT, value_parser = relay_port)]
    port: u16,
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

fn relay_port(address: &str) -> Result<u16, String> {
    if let Ok(port) = address.parse::<u16>() {
        return Ok(port);
    }
    let address: std::net::SocketAddrV4 = address
        .parse()
        .map_err(|_| "expected a port or 127.0.0.1:PORT".to_owned())?;
    if *address.ip() != std::net::Ipv4Addr::LOCALHOST {
        return Err("the relay must listen on 127.0.0.1".to_owned());
    }
    Ok(address.port())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn policy(endpoint: &str) -> PinnedOrigins {
        ConfiguredOrigins::parse(endpoint, "")
            .unwrap()
            .pin()
            .await
            .unwrap()
    }

    async fn answer_with(request: &[u8], policy: PinnedOrigins) -> Vec<u8> {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(server, Arc::new(policy)));
        client.write_all(request).await.unwrap();
        client.shutdown().await.unwrap();
        let mut answered = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), client.read_u8()).await {
                Ok(Ok(byte)) => {
                    answered.push(byte);
                    if answered.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                _ => break,
            }
        }
        proxy.abort();
        let _ = proxy.await;
        answered
    }

    async fn answer_to(request: &[u8]) -> Vec<u8> {
        answer_with(request, PinnedOrigins(Vec::new())).await
    }

    #[tokio::test]
    async fn a_connect_without_destination_approval_does_not_dial() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let request = format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n");
        let answer = answer_to(request.as_bytes()).await;
        let dialed = tokio::time::timeout(Duration::from_millis(100), target.accept())
            .await
            .is_ok();
        assert!(
            answer.starts_with(b"HTTP/1.1 403") && !dialed,
            "unapproved target dialed={dialed}; response={}",
            String::from_utf8_lossy(&answer)
        );
    }

    #[tokio::test]
    async fn an_unreachable_approved_target_is_a_bad_gateway() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        drop(target);
        let request = format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n");
        let answer = answer_with(
            request.as_bytes(),
            policy(&format!("https://{address}")).await,
        )
        .await;
        assert!(
            answer.starts_with(b"HTTP/1.1 502"),
            "{}",
            String::from_utf8_lossy(&answer)
        );
    }

    async fn head(client: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            let byte = tokio::time::timeout(Duration::from_secs(2), client.read_u8())
                .await
                .unwrap()
                .unwrap();
            head.push(byte);
            if head.ends_with(b"\r\n\r\n") {
                return head;
            }
        }
    }

    #[tokio::test]
    async fn bytes_sent_with_the_request_reach_the_target() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let answered = tokio::spawn(async move {
            let (mut stream, _) = model.accept().await.unwrap();
            let mut early = [0u8; 4];
            stream.read_exact(&mut early).await.unwrap();
            stream.write_all(b"pong").await.unwrap();
            early
        });
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("https://{address}")).await),
        ));
        client
            .write_all(
                format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\nping").as_bytes(),
            )
            .await
            .unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let mut pong = [0; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        assert_eq!(answered.await.unwrap(), *b"ping");
        drop(client);
        proxy.await.unwrap();
    }

    #[test]
    fn policy_requires_explicit_bounded_http_origins() {
        assert!(
            ConfiguredOrigins::parse("", r#"{"env":{"MODEL_URL":"https://example.com"}}"#).is_err()
        );
        assert!(ConfiguredOrigins::parse("", r#"{"model_endpoints":true}"#).is_err());
        assert!(ConfiguredOrigins::parse("", r#"{"model_endpoints":[null]}"#).is_err());
        for endpoint in [
            "ftp://example.com",
            "https://user:pass@example.com",
            "https://example.com/#secret",
            "http://example.com:0",
        ] {
            assert!(
                ConfiguredOrigins::parse(endpoint, "").is_err(),
                "{endpoint}"
            );
        }
        let origins = ConfiguredOrigins::parse(
            "https://EXAMPLE.com/v1",
            r#"{"model_endpoints":["https://example.com:443/other","http://[::1]:8080/v1"]}"#,
        )
        .unwrap();
        assert_eq!(origins.0.len(), 2);
        assert_eq!(origins.0[0].host, "example.com");
        assert_eq!(origins.0[0].port, 443);
        assert_eq!(origins.0[1].host, "::1");
    }

    #[tokio::test]
    async fn wrong_port_host_and_malformed_requests_never_dial() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let approved = format!("https://{address}");
        let wrong_port = if address.port() == 65535 {
            65534
        } else {
            address.port() + 1
        };
        let requests = [
            format!(
                "CONNECT {address} HTTP/1.1\r\nHost: other.invalid:{}\r\n\r\n",
                address.port()
            ),
            format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nHost: {address}\r\n\r\n"),
            format!("CONNECT {address} HTTP/1.1\r\n\r\n"),
            format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 1\r\n\r\nx"),
            format!(
                "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            format!("CONNECT http://{address}/ HTTP/1.1\r\nHost: {address}\r\n\r\n"),
            format!("CONNECT {address}/path HTTP/1.1\r\nHost: {address}\r\n\r\n"),
            format!("CONNECT user@{address} HTTP/1.1\r\nHost: {address}\r\n\r\n"),
            format!("CONNECT 127.0.0.1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
            format!(
                "CONNECT 127.0.0.1:{wrong_port} HTTP/1.1\r\nHost: 127.0.0.1:{wrong_port}\r\n\r\n"
            ),
            format!(
                "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            format!(
                "CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx"
            ),
            format!("GET / HTTP/1.1\r\nHost: {address}\r\n\r\n"),
            format!("GET http://{address}/ HTTP/1.1\r\nHost: {address}\r\n\r\n"),
        ];
        for request in requests {
            let answer = answer_with(request.as_bytes(), policy(&approved).await).await;
            assert!(!answer.starts_with(b"HTTP/1.1 200"), "{request}");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn parser_bounds_and_cut_short_heads_never_dial() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let prefix = format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n");
        for suffix in [
            format!("X-Large: {}\r\n\r\n", "x".repeat(MAX_HEAD)),
            format!("{}\r\n", "X-Small: x\r\n".repeat(MAX_HEADERS)),
            String::new(),
        ] {
            let request = format!("{prefix}{suffix}");
            let answer = answer_with(
                request.as_bytes(),
                policy(&format!("https://{address}")).await,
            )
            .await;
            assert!(!answer.starts_with(b"HTTP/1.1 200"));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unfinished_headers_have_a_deadline_without_outbound_dialing() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("https://{address}")).await),
        ));
        client
            .write_all(
                format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nX-Incomplete: ")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(
            HEAD_TIMEOUT + Duration::from_secs(2),
            client.read_to_end(&mut answer),
        )
        .await
        .expect("header deadline closes the connection");
        assert!(!answer.starts_with(b"HTTP/1.1 200"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), model.accept())
                .await
                .is_err()
        );
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn dialing_uses_only_the_frozen_numeric_snapshot() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let pinned = PinnedOrigins(vec![PinnedOrigin {
            origin: Origin::parse("https://not-resolvable.invalid:443").unwrap(),
            addresses: vec![address],
        }]);
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(server, Arc::new(pinned)));
        client.write_all(b"CONNECT not-resolvable.invalid:443 HTTP/1.1\r\nHost: not-resolvable.invalid:443\r\n\r\n").await.unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let (stream, _) = target.accept().await.unwrap();
        drop(stream);
        drop(client);
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn prepared_dns_and_explicit_additional_endpoints_reach_their_model() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = model.local_addr().unwrap().port();
        let options =
            serde_json::json!({"model_endpoints": [format!("http://localhost:{port}/v1")]})
                .to_string();
        let pinned = ConfiguredOrigins::parse("https://127.0.0.1:1", &options)
            .unwrap()
            .pin()
            .await
            .unwrap();
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(server, Arc::new(pinned)));
        client
            .write_all(
                format!("CONNECT localhost:{port} HTTP/1.1\r\nHost: LOCALHOST:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), model.accept())
            .await
            .unwrap()
            .unwrap();
        drop(client);
        drop(stream);
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn a_relay_client_reaches_the_target_behind_the_proxy() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let model_task = tokio::spawn(async move {
            let (mut stream, _) = model.accept().await.unwrap();
            let mut buffer = [0; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(b"pong").await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let proxy = open(
            &tokio::runtime::Handle::current(),
            socket.clone(),
            policy(&format!("https://{address}")).await,
        )
        .unwrap();
        let relay = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = relay.local_addr().unwrap().port();
        let relay = tokio::spawn(forward(relay, socket));
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client
            .write_all(
                format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\nping").as_bytes(),
            )
            .await
            .unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let mut body = [0; 4];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"pong");
        assert_eq!(proxy.bridge.port, RELAY_PORT);
        proxy.close();
        relay.abort();
        let _ = relay.await;
        model_task.await.unwrap();
    }

    #[tokio::test]
    async fn close_and_drop_terminate_owned_tunnels() {
        for explicit in [true, false] {
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = target.local_addr().unwrap();
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("proxy.sock");
            let proxy = open(
                &tokio::runtime::Handle::current(),
                socket.clone(),
                policy(&format!("https://{address}")).await,
            )
            .unwrap();
            let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
            client
                .write_all(
                    format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
            let (mut upstream, _) = target.accept().await.unwrap();
            if explicit {
                proxy.close();
            }
            drop(proxy);
            assert!(!socket.exists());
            let mut byte = [0; 1];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        }
    }

    #[tokio::test]
    async fn ordinary_http_streams_uploads_and_strips_proxy_headers() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let (seen, received) = tokio::sync::oneshot::channel();
        let seen = Arc::new(Mutex::new(Some(seen)));
        let model_task = tokio::spawn(async move {
            let (stream, _) = model.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |mut request: Request<Incoming>| {
                        let seen = Arc::clone(&seen);
                        async move {
                            assert_eq!(request.uri(), "/v1/chat?q=1");
                            assert_eq!(request.headers()[header::HOST], address.to_string());
                            assert!(!request.headers().contains_key("proxy-authorization"));
                            assert!(!request.headers().contains_key("x-private-hop"));
                            assert!(!request.headers().contains_key("proxy-connection"));
                            assert_eq!(
                                request.headers()[header::AUTHORIZATION],
                                "Bearer model-key"
                            );
                            let first = request
                                .body_mut()
                                .frame()
                                .await
                                .unwrap()
                                .unwrap()
                                .into_data()
                                .unwrap();
                            assert_eq!(first, "ping");
                            seen.lock().unwrap().take().unwrap().send(()).unwrap();
                            let rest = request.into_body().collect().await.unwrap().to_bytes();
                            assert_eq!(rest, "pong");
                            Ok::<_, std::convert::Infallible>(Response::new(
                                http_body_util::Full::new(Bytes::from_static(b"uploaded")),
                            ))
                        }
                    }),
                )
                .await
                .unwrap();
        });
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("http://{address}")).await),
        ));
        client.write_all(format!("POST http://{address}/v1/chat?q=1 HTTP/1.1\r\nHost: {address}\r\nConnection: x-private-hop, Host\r\nX-Private-Hop: secret\r\nProxy-Authorization: Basic secret\r\nProxy-Connection: keep-alive\r\nAuthorization: Bearer model-key\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nping\r\n").as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        client.write_all(b"4\r\npong\r\n0\r\n\r\n").await.unwrap();
        client.shutdown().await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 200"));
        assert!(answer.ends_with(b"uploaded"));
        proxy.await.unwrap();
        model_task.await.unwrap();
    }

    #[tokio::test]
    async fn ordinary_http_preserves_chunked_and_fixed_length_get_bodies() {
        for (framing, payload) in [
            ("Content-Length: 4", "ping"),
            ("Transfer-Encoding: chunked", "4\r\nping\r\n0\r\n\r\n"),
        ] {
            let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = model.local_addr().unwrap();
            let (seen, received) = tokio::sync::oneshot::channel();
            let seen = Arc::new(Mutex::new(Some(seen)));
            let model_task = tokio::spawn(async move {
                let (stream, _) = model.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request: Request<Incoming>| {
                            let seen = Arc::clone(&seen);
                            async move {
                                assert_eq!(request.method(), Method::GET);
                                let body = request.into_body().collect().await.unwrap().to_bytes();
                                seen.lock().unwrap().take().unwrap().send(body).unwrap();
                                Ok::<_, std::convert::Infallible>(Response::new(
                                    Empty::<Bytes>::new(),
                                ))
                            }
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
            let proxy = tokio::spawn(connect(
                server,
                Arc::new(policy(&format!("http://{address}")).await),
            ));
            client.write_all(format!("GET http://{address}/search HTTP/1.1\r\nHost: {address}\r\n{framing}\r\n\r\n{payload}").as_bytes()).await.unwrap();
            client.shutdown().await.unwrap();
            let body = tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap();
            let mut answer = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut answer))
                .await
                .unwrap()
                .unwrap();
            proxy.await.unwrap();
            model_task.await.unwrap();
            assert!(answer.starts_with(b"HTTP/1.1 200"));
            assert_eq!(
                body, "ping",
                "the upstream must receive a GET body with {framing}"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_http_preserves_safe_request_trailers_only() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let (seen, received) = tokio::sync::oneshot::channel();
        let seen = Arc::new(Mutex::new(Some(seen)));
        let model_task = tokio::spawn(async move {
            let (stream, _) = model.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |request: Request<Incoming>| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let declaration = request.headers().get(header::TRAILER).cloned();
                            let body = request.into_body().collect().await.unwrap();
                            let trailers = body.trailers().cloned().unwrap_or_default();
                            seen.lock()
                                .unwrap()
                                .take()
                                .unwrap()
                                .send((declaration, trailers, body.to_bytes()))
                                .unwrap();
                            Ok::<_, std::convert::Infallible>(Response::new(Empty::<Bytes>::new()))
                        }
                    }),
                )
                .await
                .unwrap();
        });
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("http://{address}")).await),
        ));
        client.write_all(format!("POST http://{address}/upload HTTP/1.1\r\nHost: {address}\r\nConnection: x-private-hop\r\nTransfer-Encoding: chunked\r\nTrailer: x-model-digest, x-private-hop, proxy-authorization\r\n\r\n4\r\nping\r\n0\r\nx-model-digest: digest-value\r\nx-private-hop: secret-hop\r\nproxy-authorization: secret-proxy\r\n\r\n").as_bytes()).await.unwrap();
        client.shutdown().await.unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        proxy.await.unwrap();
        model_task.await.unwrap();
        let (declaration, trailers, body) = observed;
        assert_eq!(body, "ping");
        assert_eq!(
            trailers.get("x-model-digest").map(|v| v.as_bytes()),
            Some(b"digest-value".as_slice())
        );
        assert!(!trailers.contains_key("x-private-hop"));
        assert!(!trailers.contains_key("proxy-authorization"));
        let declaration = declaration.expect("safe upload trailers must be declared");
        assert_eq!(declaration, "x-model-digest");
    }

    #[tokio::test]
    async fn ordinary_http_preserves_safe_response_trailers_only() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let model_task = tokio::spawn(async move {
            let (mut stream, _) = model.accept().await.unwrap();
            let _ = head(&mut stream).await;
            stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: x-private-hop\r\nTransfer-Encoding: chunked\r\nTrailer: x-model-usage, x-private-hop, proxy-authorization\r\n\r\n4\r\ndata\r\n0\r\nx-model-usage: 7\r\nx-private-hop: secret-hop\r\nproxy-authorization: secret-proxy\r\n\r\n").await.unwrap();
        });
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("http://{address}")).await),
        ));
        client.write_all(format!("GET http://{address}/stream HTTP/1.1\r\nHost: {address}\r\nTE: trailers\r\n\r\n").as_bytes()).await.unwrap();
        client.shutdown().await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        proxy.await.unwrap();
        model_task.await.unwrap();
        let answer = String::from_utf8(answer).unwrap().to_ascii_lowercase();
        assert!(answer.starts_with("http/1.1 200"));
        assert!(
            answer.contains("\r\ntrailer: x-model-usage\r\n"),
            "{answer}"
        );
        assert!(answer.contains("\r\n0\r\nx-model-usage: 7\r\n"), "{answer}");
        assert!(!answer.contains("x-private-hop"), "{answer}");
        assert!(!answer.contains("proxy-authorization"), "{answer}");
        assert!(!answer.contains("secret-"), "{answer}");
    }

    #[tokio::test]
    async fn ordinary_http_streams_responses_without_following_redirects() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let forbidden = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/secret", forbidden.local_addr().unwrap());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = Arc::new(Mutex::new(Some(released)));
        let model_task = tokio::spawn(async move {
            let (stream, _) = model.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service_fn(move |_request: Request<Incoming>| {
                let released = released.lock().unwrap().take().unwrap();
                let location = location.clone();
                async move {
                    let body = http_body_util::StreamBody::new(async_stream::stream! {
                        yield Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(Bytes::from_static(b"early")));
                        released.await.unwrap();
                        yield Ok(hyper::body::Frame::data(Bytes::from_static(b"late")));
                    });
                    let mut response = Response::new(body);
                    *response.status_mut() = StatusCode::FOUND;
                    response.headers_mut().insert(header::LOCATION, location.parse().unwrap());
                    response.headers_mut().insert(header::CONNECTION, "x-private-hop".parse().unwrap());
                    response.headers_mut().insert("x-private-hop", "secret".parse().unwrap());
                    response.headers_mut().insert("proxy-authenticate", "secret".parse().unwrap());
                    Ok::<_, std::convert::Infallible>(response)
                }
            })).await.unwrap();
        });
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("http://{address}")).await),
        ));
        let (mut sender, driver) =
            hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(TokioIo::new(client))
                .await
                .unwrap();
        let driver = tokio::spawn(driver);
        let mut response = sender
            .send_request(
                Request::builder()
                    .uri(format!("http://{address}/redirect"))
                    .header(header::HOST, address.to_string())
                    .body(Empty::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(!response.headers().contains_key("x-private-hop"));
        assert!(!response.headers().contains_key("proxy-authenticate"));
        let first = tokio::time::timeout(Duration::from_secs(2), response.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first, "early");
        release.send(()).unwrap();
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "late"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), forbidden.accept())
                .await
                .is_err()
        );
        drop(sender);
        driver.await.unwrap().unwrap();
        proxy.await.unwrap();
        model_task.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_launches_have_independent_authorization_and_cleanup() {
        let model_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = model_a.local_addr().unwrap();
        let b = model_b.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let proxy_a = open(
            &tokio::runtime::Handle::current(),
            root.path().join("a.sock"),
            policy(&format!("https://{a}")).await,
        )
        .unwrap();
        let proxy_b = open(
            &tokio::runtime::Handle::current(),
            root.path().join("b.sock"),
            policy(&format!("https://{b}")).await,
        )
        .unwrap();
        let mut denied = tokio::net::UnixStream::connect(&proxy_a.bridge.socket)
            .await
            .unwrap();
        denied
            .write_all(format!("CONNECT {b} HTTP/1.1\r\nHost: {b}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(head(&mut denied).await.starts_with(b"HTTP/1.1 403"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), model_b.accept())
                .await
                .is_err()
        );
        let mut client_b = tokio::net::UnixStream::connect(&proxy_b.bridge.socket)
            .await
            .unwrap();
        client_b
            .write_all(format!("CONNECT {b} HTTP/1.1\r\nHost: {b}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(head(&mut client_b).await.starts_with(b"HTTP/1.1 200"));
        let (mut upstream_b, _) = model_b.accept().await.unwrap();
        proxy_a.close();
        client_b.write_all(b"still alive").await.unwrap();
        let mut bytes = [0; 11];
        upstream_b.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"still alive");
        proxy_b.close();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), upstream_b.read_u8())
                .await
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn close_terminates_ordinary_http_client_drivers_and_streams() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let proxy = open(
            &tokio::runtime::Handle::current(),
            socket.clone(),
            policy(&format!("http://{address}")).await,
        )
        .unwrap();
        let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        client
            .write_all(
                format!("GET http://{address}/stream HTTP/1.1\r\nHost: {address}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let (mut upstream, _) = model.accept().await.unwrap();
        let received = head(&mut upstream).await;
        assert!(received.starts_with(b"GET /stream HTTP/1.1"));
        upstream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nlive\r\n")
            .await
            .unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        proxy.close();
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut remaining))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn connection_capacity_rejects_excess_clients_without_dialing() {
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let proxy = open(
            &tokio::runtime::Handle::current(),
            socket.clone(),
            policy(&format!("https://{address}")).await,
        )
        .unwrap();
        let mut clients = Vec::new();
        let mut upstreams = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
            client
                .write_all(
                    format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
            upstreams.push(model.accept().await.unwrap().0);
            clients.push(client);
        }
        let mut excess = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let _ = excess
            .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await;
        let mut byte = [0; 1];
        let closed = tokio::time::timeout(Duration::from_secs(2), excess.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0) | Err(_)));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), model.accept())
                .await
                .is_err()
        );
        proxy.close();
        for mut stream in upstreams {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        }
        drop(clients);
    }

    #[tokio::test]
    async fn relay_abort_terminates_accepted_connections() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("relay.sock");
        let unix = tokio::net::UnixListener::bind(&socket).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(forward(listener, socket));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut upstream, _) = unix.accept().await.unwrap();
        client.write_all(b"alive").await.unwrap();
        let mut marker = [0; 5];
        upstream.read_exact(&mut marker).await.unwrap();
        assert_eq!(&marker, b"alive");
        task.abort();
        let _ = task.await;
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn explicit_ipv6_origin_can_tunnel() {
        let model = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = model.local_addr().unwrap();
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(
            server,
            Arc::new(policy(&format!("https://{address}/v1")).await),
        ));
        client
            .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let (stream, _) = model.accept().await.unwrap();
        drop(client);
        drop(stream);
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_owns_its_socket_until_close_or_drop() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let runtime = tokio::runtime::Handle::current();
        let proxy = open(&runtime, socket.clone(), policy("http://127.0.0.1:1").await).unwrap();
        assert!(socket.exists());
        drop(proxy);
        assert!(!socket.exists());
        let proxy = open(&runtime, socket.clone(), policy("http://127.0.0.1:1").await).unwrap();
        proxy.close();
        std::fs::write(&socket, "not our socket").unwrap();
        drop(proxy);
        assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not our socket");
        assert!(open(&runtime, socket.clone(), policy("http://127.0.0.1:1").await).is_err());
        assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not our socket");
    }
}
