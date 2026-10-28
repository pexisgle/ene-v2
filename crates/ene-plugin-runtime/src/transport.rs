//! Remoc channels carry typed control messages with bounded serialized items.
use crate::limits::{CHANNEL_BUFFER, MAX_MESSAGE_BYTES};
use ene_plugin::{HostMessage, WorkerMessage};
use remoc::ConnectExt;
use remoc::rch::mpsc;
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

type Codec = remoc::codec::MessagePack;
type Receiver<T> = mpsc::Receiver<T, Codec, CHANNEL_BUFFER, MAX_MESSAGE_BYTES>;

#[derive(Serialize, Deserialize)]
struct Endpoint {
    commands: Receiver<HostMessage>,
    events: mpsc::Sender<WorkerMessage, Codec>,
}

pub struct Writer<T>(mpsc::Sender<T, Codec>);
pub struct Reader<T>(Receiver<T>);

fn broken(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

impl<T: remoc::RemoteSend + Clone> Writer<T> {
    pub async fn send(&mut self, message: &T) -> io::Result<()> {
        self.0
            .send(message.clone())
            .await
            .map_err(broken)?
            .await
            .map_err(broken)
    }
}
impl<T: remoc::RemoteSend> Reader<T> {
    pub async fn receive(&mut self) -> io::Result<Option<T>> {
        self.0.recv().await.map_err(broken)
    }
}

pub async fn host<R, W>(
    input: R,
    output: W,
) -> io::Result<(Writer<HostMessage>, Reader<WorkerMessage>)>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (mut commands, command_rx) = mpsc::with_local_buffer::<HostMessage, Codec>(CHANNEL_BUFFER);
    let (mut events, event_rx) = mpsc::with_local_buffer::<WorkerMessage, Codec>(CHANNEL_BUFFER);
    commands.set_max_item_size(MAX_MESSAGE_BYTES);
    events.set_max_item_size(MAX_MESSAGE_BYTES);
    let endpoint = Endpoint {
        commands: command_rx.set_max_item_size(),
        events,
    };
    remoc::Connect::io::<_, _, Endpoint, Endpoint, remoc::codec::Default>(
        remoc::Cfg::default(),
        input,
        output,
    )
    .provide(endpoint)
    .await
    .map_err(broken)?;
    Ok((Writer(commands), Reader(event_rx.set_max_item_size())))
}

pub async fn worker<R, W>(
    input: R,
    output: W,
) -> io::Result<(Reader<HostMessage>, Writer<WorkerMessage>)>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let endpoint: Endpoint = remoc::Connect::io::<_, _, Endpoint, Endpoint, remoc::codec::Default>(
        remoc::Cfg::default(),
        input,
        output,
    )
    .consume()
    .await
    .map_err(broken)?;
    Ok((Reader(endpoint.commands), Writer(endpoint.events)))
}
