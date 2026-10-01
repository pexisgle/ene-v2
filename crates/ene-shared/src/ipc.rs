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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn fragmented_unicode_and_back_to_back_lines_are_preserved() {
        let (mut sender, stream) = tokio::io::duplex(64);
        let mut reader = Reader::new(stream);
        let send = tokio::spawn(async move {
            for byte in "\"日本語🙂\"\n42\n".bytes() {
                sender.write_all(&[byte]).await.unwrap();
            }
        });
        assert_eq!(
            reader.receive::<String>().await.unwrap().unwrap(),
            "日本語🙂"
        );
        assert_eq!(reader.receive::<u32>().await.unwrap(), Some(42));
        send.await.unwrap();
        assert!(reader.receive::<u32>().await.unwrap().is_none());
    }
    #[tokio::test]
    async fn limit_includes_newline_and_rejects_incomplete_or_invalid_json() {
        let accepted = format!("\"{}\"\n", "a".repeat(MAX_IPC_LINE - 3));
        let mut reader = Reader::new(accepted.as_bytes());
        assert_eq!(
            reader.receive::<String>().await.unwrap().unwrap().len(),
            MAX_IPC_LINE - 3
        );
        let oversized = format!("\"{}\"\n", "a".repeat(MAX_IPC_LINE - 2));
        assert!(matches!(
            Reader::new(oversized.as_bytes()).receive::<String>().await,
            Err(Error::TooLong)
        ));
        assert!(matches!(
            Reader::new(b"42".as_slice()).receive::<u32>().await,
            Err(Error::Unterminated)
        ));
        assert!(matches!(
            Reader::new(b"bad\n".as_slice()).receive::<u32>().await,
            Err(Error::Json(_))
        ));
        assert!(matches!(
            Writer::new(tokio::io::sink())
                .send(&"a".repeat(MAX_IPC_LINE - 2))
                .await,
            Err(Error::TooLong)
        ));
    }
    #[tokio::test]
    async fn cancelled_partial_write_is_discarded_by_closing_the_pipe() {
        let (sender, mut receiver) = tokio::io::duplex(8);
        let mut writer = Writer::new(sender);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                writer.send(&"日本語".repeat(100))
            )
            .await
            .is_err()
        );
        drop(writer);
        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).await.unwrap();
        assert!(!bytes.is_empty());
        assert!(!bytes.contains(&b'\n'));
    }
}
