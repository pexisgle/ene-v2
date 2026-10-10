//! Audio uses generated protobuf messages on a bounded, non-reconnecting socket.
use super::*;
use buffa::Message;
use ene_api_types::rpc as w;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use http::StatusCode;
use tokio::{net::TcpStream, sync::watch, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message as Frame, client::IntoClientRequest, protocol::WebSocketConfig},
};
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub enum AudioEvent {
    Chunk(w::AudioChunk),
    Ended,
    Failed(w::AudioFailure),
}
pub struct AudioStream {
    input: AudioInput,
    output: AudioOutput,
}
pub struct AudioInput {
    socket: Option<SplitSink<Socket, Frame>>,
    session: String,
    sequence: u64,
    ended: bool,
    closed: watch::Sender<bool>,
}
pub struct AudioOutput {
    socket: Option<SplitStream<Socket>>,
    session: String,
    sequence: u64,
    closed: watch::Sender<bool>,
}
impl ApiClient {
    pub async fn prepare_audio(
        &self,
        companion: &str,
        device: &str,
    ) -> Result<w::AudioStart, Error> {
        Ok(self
            .host_rpc()
            .prepare_audio_with_options(
                w::PrepareAudioRequest {
                    companion_id: companion.into(),
                    device_id: device.into(),
                    codec: "pcm_s16le".into(),
                    sample_rate: 24_000,
                    channels: 1,
                    ..Default::default()
                },
                super::rpc::options(5),
            )
            .await
            .map_err(super::rpc::rpc_error)?
            .into_owned())
    }
    pub async fn start_audio(&self, start: w::AudioStart) -> Result<AudioStream, Error> {
        let mut url = self.0.endpoint.clone();
        url.set_path("/audio");
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|_| Error::InvalidConnection)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::InvalidConnection)?;
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.0.bearer.clone());
        let config = WebSocketConfig::default()
            .write_buffer_size(0)
            .max_write_buffer_size(128 * 1024)
            .max_message_size(Some(64 * 1024))
            .max_frame_size(Some(64 * 1024));
        let (mut socket,_)=timeout(Duration::from_secs(5),connect_async_with_config(request,Some(config),false)).await.map_err(|_|Error::Transport)?.map_err(|error| {
            if matches!(error,tokio_tungstenite::tungstenite::Error::Http(ref response) if response.status()==StatusCode::UNAUTHORIZED) {Error::Unauthorized}else{Error::Transport}
        })?;
        let bytes = w::AudioFrame {
            frame: Some(w::audio_frame::Frame::Start(Box::new(start.clone()))),
            ..Default::default()
        }
        .encode_to_vec();
        if bytes.len() > 64 * 1024 {
            return Err(Error::ResourceLimit);
        }
        timeout(
            Duration::from_secs(2),
            socket.send(Frame::Binary(bytes.into())),
        )
        .await
        .map_err(|_| Error::Transport)?
        .map_err(|_| Error::Transport)?;
        let frame = timeout(Duration::from_secs(5), async {
            loop {
                match socket.next().await {
                    Some(Ok(Frame::Binary(bytes))) => {
                        return w::AudioFrame::decode_from_slice(&bytes)
                            .map_err(|_| Error::Protocol)?
                            .frame
                            .ok_or(Error::Protocol);
                    }
                    Some(Ok(Frame::Ping(_))) => {
                        socket.flush().await.map_err(|_| Error::Transport)?
                    }
                    Some(Ok(Frame::Pong(_))) => {}
                    _ => return Err(Error::Transport),
                }
            }
        })
        .await
        .map_err(|_| Error::Transport)??;
        match frame {
            w::audio_frame::Frame::Start(ack) if *ack == start => {}
            w::audio_frame::Frame::Failure(failure) if failure.session_id == start.session_id => {
                return Err(Error::Api(Box::new(ApiFailure {
                    status: 422,
                    body: ApiErrorBody {
                        request_id: start.session_id,
                        error: Failure {
                            code: failure.code,
                            message: "音声セッションを開始できません".into(),
                            details: serde_json::json!({}),
                        },
                    },
                })));
            }
            _ => return Err(Error::Protocol),
        }
        let (input, output) = socket.split();
        let closed = watch::channel(false).0;
        Ok(AudioStream {
            input: AudioInput {
                socket: Some(input),
                session: start.session_id.clone(),
                sequence: 0,
                ended: false,
                closed: closed.clone(),
            },
            output: AudioOutput {
                socket: Some(output),
                session: start.session_id,
                sequence: 0,
                closed,
            },
        })
    }
}
impl AudioStream {
    /// Split input/output so microphone capture and playback run concurrently.
    pub fn into_split(self) -> (AudioInput, AudioOutput) {
        (self.input, self.output)
    }
    pub async fn send_chunk(&mut self, pcm: Vec<u8>) -> Result<(), Error> {
        self.input.send_chunk(pcm).await
    }
    pub async fn end_input(&mut self) -> Result<(), Error> {
        self.input.end_input().await
    }
    pub async fn next(&mut self) -> Result<Option<AudioEvent>, Error> {
        self.output.next().await
    }
    pub async fn close(self) -> Result<(), Error> {
        self.input.close().await
    }
}
impl AudioInput {
    async fn send(&mut self, frame: w::audio_frame::Frame) -> Result<(), Error> {
        if *self.closed.borrow() {
            return Err(Error::Protocol);
        }
        let bytes = w::AudioFrame {
            frame: Some(frame),
            ..Default::default()
        }
        .encode_to_vec();
        if bytes.len() > 64 * 1024 {
            return Err(Error::ResourceLimit);
        }
        let socket = self.socket.as_mut().ok_or(Error::Protocol)?;
        let result = timeout(
            Duration::from_secs(2),
            socket.send(Frame::Binary(bytes.into())),
        )
        .await
        .map_err(|_| Error::Transport)
        .and_then(|result| result.map_err(|_| Error::Transport));
        if result.is_err() {
            self.closed.send_replace(true);
            self.socket = None;
        }
        result
    }
    pub async fn send_chunk(&mut self, pcm: Vec<u8>) -> Result<(), Error> {
        if self.ended || pcm.is_empty() || pcm.len() > 16 * 1024 || !pcm.len().is_multiple_of(2) {
            return Err(Error::Protocol);
        }
        let sequence = self.sequence;
        self.sequence = sequence.checked_add(1).ok_or(Error::ResourceLimit)?;
        self.send(w::audio_frame::Frame::Chunk(Box::new(w::AudioChunk {
            session_id: self.session.clone(),
            sequence,
            data: pcm,
            ..Default::default()
        })))
        .await
    }
    /// Finish microphone input and continue receiving synthesized output.
    pub async fn end_input(&mut self) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Protocol);
        }
        self.ended = true;
        self.send(w::audio_frame::Frame::End(Box::new(w::AudioEnd {
            session_id: self.session.clone(),
            ..Default::default()
        })))
        .await
    }
    /// Invalidate locally before sending Close, discarding queued old output.
    pub async fn close(mut self) -> Result<(), Error> {
        self.closed.send_replace(true);
        self.ended = true;
        if let Some(mut socket) = self.socket.take() {
            timeout(Duration::from_secs(1), socket.close())
                .await
                .map_err(|_| Error::Transport)?
                .map_err(|_| Error::Transport)?;
        }
        Ok(())
    }
}
impl Drop for AudioInput {
    fn drop(&mut self) {
        if !self.ended {
            self.closed.send_replace(true);
        }
    }
}
impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}
impl AudioOutput {
    pub async fn next(&mut self) -> Result<Option<AudioEvent>, Error> {
        if *self.closed.borrow() {
            self.socket = None;
            return Ok(None);
        }
        let Some(socket) = self.socket.as_mut() else {
            return Ok(None);
        };
        let mut cancelled = self.closed.subscribe();
        let frame = loop {
            let incoming = tokio::select! { biased;
                _ = cancelled.changed() => {
                    self.socket = None;
                    return Ok(None);
                },
                incoming = socket.next() => incoming,
            };
            match incoming {
                Some(Ok(Frame::Binary(bytes))) => {
                    match w::AudioFrame::decode_from_slice(&bytes)
                        .ok()
                        .and_then(|message| message.frame)
                    {
                        Some(frame) => break frame,
                        None => {
                            self.socket = None;
                            self.closed.send_replace(true);
                            return Err(Error::Protocol);
                        }
                    }
                }
                Some(Ok(Frame::Ping(_))) | Some(Ok(Frame::Pong(_))) => {}
                _ => {
                    self.socket = None;
                    self.closed.send_replace(true);
                    return Err(Error::Transport);
                }
            }
        };
        if *self.closed.borrow() {
            self.socket = None;
            return Ok(None);
        }
        match frame {
            w::audio_frame::Frame::Chunk(chunk)
                if chunk.session_id == self.session
                    && chunk.sequence == self.sequence
                    && !chunk.data.is_empty()
                    && chunk.data.len() <= 16 * 1024
                    && chunk.data.len().is_multiple_of(2) =>
            {
                let Some(sequence) = self.sequence.checked_add(1) else {
                    self.socket = None;
                    self.closed.send_replace(true);
                    return Err(Error::ResourceLimit);
                };
                self.sequence = sequence;
                Ok(Some(AudioEvent::Chunk(*chunk)))
            }
            w::audio_frame::Frame::End(end) if end.session_id == self.session => {
                self.socket = None;
                self.closed.send_replace(true);
                Ok(Some(AudioEvent::Ended))
            }
            w::audio_frame::Frame::Failure(failure) if failure.session_id == self.session => {
                self.socket = None;
                self.closed.send_replace(true);
                Ok(Some(AudioEvent::Failed(*failure)))
            }
            _ => {
                self.socket = None;
                self.closed.send_replace(true);
                Err(Error::Protocol)
            }
        }
    }
}
