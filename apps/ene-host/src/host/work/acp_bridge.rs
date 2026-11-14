//! The bridge a network-isolated ACP child uses to reach its model.
//!
//! `network: isolated` puts the child in its own network namespace, so the only
//! route it has is loopback. The host serves an HTTP CONNECT proxy on a unix
//! socket it binds into that namespace, and this binary runs in `acp-relay`
//! mode inside it: the relay listens on loopback, forwards every connection to
//! the socket, and runs the agent itself. The agent finds the proxy through
//! `HTTPS_PROXY`, so a client that ignores that variable has no network at all
//! rather than an open one.
use std::{ffi::OsString, io, path::PathBuf, process::Stdio};

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

/// Serves HTTP CONNECT on `socket` until the returned handle is aborted.
#[cfg(unix)]
pub(crate) fn open(
    runtime: &tokio::runtime::Handle,
    socket: PathBuf,
) -> io::Result<(Bridge, tokio::task::AbortHandle)> {
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let task = runtime.spawn(async move {
        let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
            return;
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = connect(stream).await;
            });
        }
    });
    Ok((
        Bridge {
            socket,
            port: RELAY_PORT,
        },
        task.abort_handle(),
    ))
}

/// Only a unix host can serve the socket, and only bwrap produces the sandbox
/// that needs it; elsewhere the launch stays unisolated.
#[cfg(not(unix))]
pub(crate) fn open(
    _runtime: &tokio::runtime::Handle,
    _socket: PathBuf,
) -> io::Result<(Bridge, tokio::task::AbortHandle)> {
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
    let Some((bridge, command)) = parse_relay(arguments) else {
        eprintln!("acp-relay: usage: --socket PATH --listen ADDR -- PROGRAM [ARGS...]");
        return 2;
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
        .block_on(async { tokio::net::TcpListener::bind(("127.0.0.1", bridge.port)).await })
    {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("acp-relay: {error}");
            return 1;
        }
    };
    #[cfg(unix)]
    {
        let socket = bridge.socket.clone();
        runtime.spawn(forward(listener, socket));
    }
    #[cfg(not(unix))]
    drop(listener);
    let Some((program, args)) = command.split_first() else {
        eprintln!("acp-relay: no program");
        return 2;
    };
    let status = std::process::Command::new(program)
        .args(args)
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

/// The socket and the program of a relay command line.
fn parse_relay(arguments: &[OsString]) -> Option<(Bridge, Vec<OsString>)> {
    let mut socket: Option<PathBuf> = None;
    let mut port = RELAY_PORT;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_str()?;
        match argument {
            "--" => {
                let command: Vec<OsString> = arguments[index + 1..].to_vec();
                return Some((
                    Bridge {
                        socket: socket?,
                        port,
                    },
                    command,
                ));
            }
            "--socket" => {
                socket = Some(PathBuf::from(arguments.get(index + 1)?));
                index += 2;
            }
            "--listen" => {
                let address = arguments.get(index + 1)?.to_str()?;
                port = address.rsplit(':').next()?.parse().ok()?;
                index += 2;
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
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
        let (bridge, proxy) = open(&tokio::runtime::Handle::current(), socket.clone()).unwrap();

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
        proxy.abort();
    }

    #[test]
    fn a_relay_command_line_names_the_socket_the_port_and_the_program() {
        let parsed = parse_relay(&args(&[
            "--socket",
            "/run/ene/proxy.sock",
            "--listen",
            "127.0.0.1:3128",
            "--",
            "agent",
            "--stdio",
        ]))
        .expect("a relay command line");
        assert_eq!(parsed.0.socket, Path::new("/run/ene/proxy.sock"));
        assert_eq!(parsed.0.port, 3128);
        assert_eq!(parsed.1, args(&["agent", "--stdio"]));
    }

    #[test]
    fn a_relay_command_line_without_a_program_is_refused() {
        assert!(
            parse_relay(&args(&[
                "--socket",
                "/tmp/x.sock",
                "--listen",
                "127.0.0.1:1"
            ]))
            .is_none()
        );
        assert!(parse_relay(&args(&["agent"])).is_none());
    }
}
