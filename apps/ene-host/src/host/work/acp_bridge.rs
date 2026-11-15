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

/// Reads one CONNECT request, dials its target and pipes the two sides.
#[cfg(unix)]
async fn connect(mut client: tokio::net::UnixStream) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Ok(());
        }
        if client.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
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
