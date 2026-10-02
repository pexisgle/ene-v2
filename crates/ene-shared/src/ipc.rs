//! Bounded NDJSON. A failed or cancelled send requires closing its connection.
use crate::avatar::MAX_IPC_LINE;
use futures_util::{SinkExt, StreamExt};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite, LinesCodec};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IPC frame: {0}")]
    Frame(#[from] tokio_util::codec::LinesCodecError),
    #[error("IPC JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IPC line exceeds 64 KiB including newline")]
    TooLong,
    #[error("IPC line is not terminated")]
    Unterminated,
}
#[derive(Default)]
struct Codec(LinesCodec);
impl Decoder for Codec {
    type Item = String;
    type Error = Error;
    fn decode(&mut self, bytes: &mut bytes::BytesMut) -> Result<Option<String>, Error> {
        if bytes
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(bytes.len())
            >= MAX_IPC_LINE
        {
            return Err(Error::TooLong);
        }
        Ok(self.0.decode(bytes)?)
    }
    fn decode_eof(&mut self, bytes: &mut bytes::BytesMut) -> Result<Option<String>, Error> {
        if let Some(line) = self.decode(bytes)? {
            return Ok(Some(line));
        }
        if bytes.is_empty() {
            Ok(None)
        } else {
            Err(Error::Unterminated)
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Frame(error.into())
    }
}
impl Encoder<String> for Codec {
    type Error = Error;
    fn encode(&mut self, line: String, bytes: &mut bytes::BytesMut) -> Result<(), Error> {
        if line.len() >= MAX_IPC_LINE {
            return Err(Error::TooLong);
        }
        Ok(self.0.encode(line, bytes)?)
    }
}
pub struct Reader<R>(FramedRead<R, Codec>);
impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(input: R) -> Self {
        Self(FramedRead::new(
            input,
            Codec(LinesCodec::new_with_max_length(MAX_IPC_LINE - 1)),
        ))
    }
    pub async fn receive<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        match self.0.next().await {
            Some(line) => Ok(Some(serde_json::from_str(&line?)?)),
            None => Ok(None),
        }
    }
}
pub struct Writer<W>(FramedWrite<W, Codec>);
impl<W: AsyncWrite + Unpin> Writer<W> {
    pub fn new(output: W) -> Self {
        Self(FramedWrite::new(
            output,
            Codec(LinesCodec::new_with_max_length(MAX_IPC_LINE - 1)),
        ))
    }
    pub async fn send<T: Serialize>(&mut self, message: &T) -> Result<(), Error> {
        let line = serde_json::to_string(message)?;
        if line.len() >= MAX_IPC_LINE {
            return Err(Error::TooLong);
        }
        self.0.send(line).await?;
        Ok(())
    }
}
