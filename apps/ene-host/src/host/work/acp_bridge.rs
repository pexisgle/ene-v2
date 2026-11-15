//! The bridge a network-isolated ACP child uses to reach its model.
//!
//! `network: isolated` puts the child in its own network namespace, so the only
//! route it has is loopback. The host serves an HTTP CONNECT proxy on a unix
//! socket it binds into that namespace, and this binary runs in `acp-relay`
//! mode inside it: the relay listens on loopback, forwards every connection to
//! the socket, and runs the agent itself. The agent finds the proxy through
//! `HTTPS_PROXY`, so a client that ignores that variable has no network at all
//! rather than an open one.
use clap::Parser;
use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
};

/// The loopback port the relay listens on inside the sandbox. The namespace is
/// private to one launch, so a fixed port cannot collide with the host.
pub(crate) const RELAY_PORT: u16 = 3128;

/// The variables a proxy-aware HTTP client reads.
pub(crate) const PROXY_VARS: [&str; 2] = ["HTTPS_PROXY", "HTTP_PROXY"];

/// Where one sandboxed launch's proxy lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bridge {
    pub(crate) socket: PathBuf,
    pub(crate) port: u16,
}

impl Bridge {
    /// The value the child sets `HTTPS_PROXY` to.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// One launch's proxy: its address and the task that serves it.
#[derive(Debug)]
pub(crate) struct Proxy {
    pub(crate) bridge: Bridge,
    handle: tokio::task::AbortHandle,
    closed: AtomicBool,
}

impl Proxy {
    /// Stops the proxy and removes its socket.
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

/// Serves HTTP CONNECT on `socket` until [`Proxy::close`].
#[cfg(unix)]
pub(crate) fn open(runtime: &tokio::runtime::Handle, socket: PathBuf) -> io::Result<Proxy> {
    // A unix socket path is limited to SUN_LEN (about 108 bytes); a longer one
    // cannot be bound or mounted into the sandbox.
    if socket.as_os_str().len() > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the ACP proxy socket path is too long: {} (set TMPDIR to a shorter directory)",
                socket.display()
            ),
        ));
    }
    let listener = {
        let _entered = runtime.enter();
        tokio::net::UnixListener::bind(&socket)?
    };
    let task = runtime.spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = connect(stream).await;
            });
        }
    });
    Ok(Proxy {
        bridge: Bridge {
            socket,
            port: RELAY_PORT,
        },
        handle: task.abort_handle(),
        closed: AtomicBool::new(false),
    })
}

/// Only a unix host can serve the socket, and only bwrap produces the sandbox
/// that needs it; elsewhere the launch stays unisolated.
#[cfg(not(unix))]
pub(crate) fn open(_runtime: &tokio::runtime::Handle, _socket: PathBuf) -> io::Result<Proxy> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the ACP proxy needs unix sockets",
    ))
}

/// The most of a request head the proxy buffers before it gives up.
#[cfg(unix)]
const MAX_HEAD: usize = 16 * 1024;

/// Reads one CONNECT request, dials its target and pipes the two sides.
///
/// The head is read in chunks and the bytes past its terminator are kept: a
/// client may start talking as soon as it has sent the request, so those bytes
/// belong to the tunnel rather than to the head.
#[cfg(unix)]
async fn connect(mut client: tokio::net::UnixStream) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut head = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(at) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        if head.len() > MAX_HEAD {
            return Ok(());
        }
        // Never read past the bound, so the most buffered is `MAX_HEAD + 1`:
        // one byte past the limit is enough to know the head was cut.
        let room = (MAX_HEAD + 1 - head.len()).min(chunk.len());
        let read = client.read(&mut chunk[..room]).await?;
        if read == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..read]);
    };
    // A head is ASCII, but a client controls these bytes: read them lossily so
    // a stray non-UTF-8 byte is a bad request rather than a panic.
    let text = String::from_utf8_lossy(&head[..end]);
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .filter(|_| text.starts_with("CONNECT "))
    else {
        client
            .write_all(b"HTTP/1.1 501 Not Implemented\r\n\r\n")
            .await?;
        return Ok(());
    };
    let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else {
        client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            .await?;
        return Ok(());
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if end < head.len() {
        upstream.write_all(&head[end..]).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// `acp-relay --socket PATH --listen 127.0.0.1:PORT -- PROGRAM [ARGS...]`, run
/// inside the sandbox. Serves loopback on `port`, runs the agent with its own
/// stdio so the ACP conversation passes through untouched, and exits with it.
pub(crate) fn relay(arguments: &[OsString]) -> i32 {
    let args = match RelayArgs::try_parse_from(arguments) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
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
    runtime.spawn(forward(listener, args.socket));
    #[cfg(not(unix))]
    drop(listener);
    let status = std::process::Command::new(&args.command[0])
        .args(&args.command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("acp-relay: {error}");
            1
        }
    }
}

/// Forwards every connection accepted on `listener` to `socket`.
#[cfg(unix)]
async fn forward(listener: tokio::net::TcpListener, socket: PathBuf) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let socket = socket.clone();
        tokio::spawn(async move {
            let Ok(mut upstream) = tokio::net::UnixStream::connect(&socket).await else {
                return;
            };
            let mut stream = stream;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        });
    }
}

#[derive(Parser)]
#[command(name = "acp-relay", no_binary_name = true, args_override_self = true)]
struct RelayArgs {
    #[arg(long, allow_hyphen_values = true)]
    socket: PathBuf,
    #[arg(
        long = "listen",
        allow_hyphen_values = true,
        value_name = "ADDR",
        default_value_t = RELAY_PORT,
        value_parser = |address: &str| address.rsplit(':').next().unwrap_or_default().parse::<u16>()
    )]
    port: u16,
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

#[cfg(test)]
mod tests {
    // Every test here drives the unix-socket proxy, so the module's items are
    // unused where it does not exist.
    #[cfg(unix)]
    use super::*;

    /// Drives one request through the proxy and returns every byte it answered
    /// before the tunnel or the close.
    #[cfg(unix)]
    async fn answer_to(request: &[u8]) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(server));
        client.write_all(request).await.unwrap();
        client.shutdown().await.unwrap();
        let mut answered = Vec::new();
        let mut buffer = [0u8; 512];
        loop {
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buffer))
                    .await;
            match read {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(read)) => {
                    answered.extend_from_slice(&buffer[..read]);
                    if answered.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        proxy.abort();
        answered
    }

    /// A client that is not tunnelling gets a refusal rather than a tunnel.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_non_connect_request_is_refused() {
        let answered = answer_to(b"GET / HTTP/1.1\r\nHost: model\r\n\r\n").await;
        assert!(
            answered.starts_with(b"HTTP/1.1 501"),
            "{}",
            String::from_utf8_lossy(&answered)
        );
    }

    /// `CONNECT` with no authority names nothing to dial.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_connect_without_a_target_is_refused() {
        let answered = answer_to(b"CONNECT\r\n\r\n").await;
        assert!(
            answered.starts_with(b"HTTP/1.1 501"),
            "{}",
            String::from_utf8_lossy(&answered)
        );
    }

    /// A target that refuses the connection is the proxy's own failure, not the
    /// client's request: 502, not 501.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreachable_target_is_a_bad_gateway() {
        let answered = answer_to(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: model\r\n\r\n").await;
        assert!(
            answered.starts_with(b"HTTP/1.1 502"),
            "{}",
            String::from_utf8_lossy(&answered)
        );
    }

    /// The head is bounded: one byte past 16 KiB closes the connection with no
    /// answer, so a client cannot make the proxy buffer without limit.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_overlong_head_closes_without_an_answer() {
        let answered = answer_to(&vec![b'x'; 16 * 1024 + 1]).await;
        assert!(
            answered.is_empty(),
            "{}",
            String::from_utf8_lossy(&answered)
        );
    }

    /// A client that stops mid-head leaves no request to answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_head_cut_short_closes_without_an_answer() {
        let answered = answer_to(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: model\r\n").await;
        assert!(
            answered.is_empty(),
            "{}",
            String::from_utf8_lossy(&answered)
        );
    }

    /// A client may send its first tunnel bytes in the same write as the
    /// request, so the proxy must not swallow what it read past the head.
    #[cfg(unix)]
    #[tokio::test]
    async fn bytes_sent_with_the_request_reach_the_target() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_port = model.local_addr().unwrap().port();
        let answered = tokio::spawn(async move {
            let (mut stream, _) = model.accept().await.unwrap();
            let mut early = [0u8; 4];
            stream.read_exact(&mut early).await.unwrap();
            early
        });

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let proxy = tokio::spawn(connect(server));
        // One write: the request head and the tunnel's first bytes together.
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{model_port} HTTP/1.1\r\nHost: model\r\n\r\nping")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut head = [0u8; 39];
        client.read_exact(&mut head).await.unwrap();
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
            "the proxy accepts the tunnel: {}",
            String::from_utf8_lossy(&head)
        );
        assert_eq!(
            answered.await.unwrap(),
            *b"ping",
            "the bytes after the head are the tunnel's, not the proxy's"
        );
        proxy.abort();
    }

    /// The path a sandboxed agent takes: TCP to the relay, a unix socket to the
    /// host's proxy, and the proxy's own TCP connection to the model.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_relay_client_reaches_the_target_behind_the_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // The "model": answers one fixed line for every connection.
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_port = model.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = model.accept().await {
                let mut buffer = [0u8; 4];
                let _ = stream.read(&mut buffer).await;
                let _ = stream.write_all(b"pong").await;
            }
        });

        // The host's proxy on its socket.
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let proxy = open(&tokio::runtime::Handle::current(), socket.clone()).unwrap();
        let bridge = proxy.bridge.clone();

        // The relay, and a client that speaks to it the way the agent would.
        let relay = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let relay_port = relay.local_addr().unwrap().port();
        tokio::spawn(forward(relay, socket));

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", relay_port))
            .await
            .unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{model_port} HTTP/1.1\r\nHost: model\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut head = [0u8; 39];
        client.read_exact(&mut head).await.unwrap();
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
            "the proxy accepts the tunnel: {}",
            String::from_utf8_lossy(&head)
        );
        client.write_all(b"ping").await.unwrap();
        let mut body = [0u8; 4];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"pong", "the tunnel carries the target's bytes");
        assert_eq!(bridge.port, RELAY_PORT);
        proxy.close();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_owns_its_socket_until_close_or_drop() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("proxy.sock");
        let runtime = tokio::runtime::Handle::current();
        let proxy = open(&runtime, socket.clone()).unwrap();
        assert!(socket.exists());
        drop(proxy);
        assert!(!socket.exists());

        let proxy = open(&runtime, socket.clone()).unwrap();
        proxy.close();
        assert!(!socket.exists());
        std::fs::write(&socket, "not our socket").unwrap();
        drop(proxy);
        assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not our socket");
        assert!(open(&runtime, socket.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not our socket");
    }
}
