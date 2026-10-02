//! Nonblocking owned standard-stream FDs; no Tokio blocking-stdin thread.
use ene_protocol::{
    avatar::{AvatarEvent, AvatarRequest},
    ipc::{Reader, Writer},
};
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};
use tokio::{
    io::unix::AsyncFd,
    io::{AsyncRead, AsyncWrite, ReadBuf},
};
use tokio_util::sync::CancellationToken;

struct Pipe(AsyncFd<OwnedFd>);
impl Pipe {
    fn new(fd: impl AsFd) -> io::Result<Self> {
        let fd = rustix::io::dup(fd)?;
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self(AsyncFd::new(fd)?))
    }
}
impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_read_ready(cx))?;
            match ready.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), buf.initialize_unfilled())?))
            {
                Ok(Ok(count)) => {
                    buf.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => {}
            }
        }
    }
}
impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_write_ready(cx))?;
            if let Ok(result) = ready.try_io(|fd| Ok(rustix::io::write(fd.get_ref(), bytes)?)) {
                return Poll::Ready(result);
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
struct Events {
    sender: tokio::sync::mpsc::Sender<AvatarEvent>,
    stop: CancellationToken,
}
static EVENTS: OnceLock<Events> = OnceLock::new();
pub fn emit(event: AvatarEvent) {
    if let Some(events) = EVENTS.get()
        && events.sender.try_send(event).is_err()
    {
        // An unread output pipe must not silently lose placement/error events.
        events.stop.cancel();
    }
}
pub struct Ipc {
    pub requests: tokio::sync::mpsc::Receiver<Option<AvatarRequest>>,
    stop: CancellationToken,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Ipc {
    pub fn new(standalone: bool) -> Self {
        let (requests, receiver) = tokio::sync::mpsc::channel(32);
        let (events, mut outgoing) = tokio::sync::mpsc::channel(64);
        let stop = CancellationToken::new();
        let _ = EVENTS.set(Events {
            sender: events,
            stop: stop.clone(),
        });
        let cancel = stop.clone();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("renderer IPC runtime");
            runtime.block_on(async move {
                let result: io::Result<()> = async {
                    let mut output = Writer::new(Pipe::new(std::io::stdout())?);
                    let output_task = tokio::spawn(async move {
                        while let Some(event) = outgoing.recv().await {
                            output.send(&event).await?;
                        }
                        Ok::<_, ene_protocol::ipc::Error>(())
                    });
                    let input = async {
                        if standalone {
                            std::future::pending::<()>().await;
                            return;
                        }
                        let input = Pipe::new(std::io::stdin());
                        match input {
                            Ok(input) => {
                                let mut input = Reader::new(input);
                                loop {
                                    match input.receive::<AvatarRequest>().await {
                                        Ok(Some(request)) => {
                                            if requests.send(Some(request)).await.is_err() {
                                                return;
                                            }
                                        }
                                        Ok(None) => break,
                                        Err(error) => {
                                            emit(AvatarEvent::Error {
                                                message: format!("IPC形式が不正です: {error}"),
                                            });
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(error) => emit(AvatarEvent::Error {
                                message: error.to_string(),
                            }),
                        }
                        let _ = requests.send(None).await;
                    };
                    tokio::pin!(input);
                    let mut output_task = output_task;
                    tokio::select! {
                        _ = cancel.cancelled() => {},
                        _ = &mut input => { cancel.cancelled().await; },
                        _ = &mut output_task => { let _ = requests.try_send(None); return Ok(()); },
                    }
                    // Permit final Stopped/Error to flush; never join a blocked pipe.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    output_task.abort();
                    let _ = output_task.await;
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("renderer IPC: {error}");
                    let _ = requests.try_send(None);
                }
            });
        });
        Self {
            requests: receiver,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Ipc {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
