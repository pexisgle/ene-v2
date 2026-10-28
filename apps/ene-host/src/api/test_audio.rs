//! Test generated wire messages without a second audio protocol or state machine.
use super::*;
use buffa::Message;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message as Frame, client::IntoClientRequest, protocol::WebSocketConfig},
};

impl ApiClient {
    pub async fn audio_socket(&self) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
        let mut url = self.endpoint.clone();
        url.set_path("/audio");
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .unwrap();
        let mut request = url.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.bearer.clone());
        let config = WebSocketConfig::default()
            .write_buffer_size(0)
            .max_write_buffer_size(128 * 1024)
            .max_message_size(Some(64 * 1024))
            .max_frame_size(Some(64 * 1024));
        timeout(
            Duration::from_secs(5),
            connect_async_with_config(request, Some(config), false),
        )
        .await
        .unwrap()
        .unwrap()
        .0
    }
}

pub async fn send(socket: &mut (impl Sink<Frame> + Unpin), frame: ene_rpc::AudioFrame) {
    assert!(frame.encoded_len() <= 64 * 1024);
    assert!(
        timeout(
            Duration::from_secs(2),
            socket.send(Frame::Binary(frame.encode_to_vec().into()))
        )
        .await
        .unwrap()
        .is_ok()
    );
}

pub async fn recv(
    socket: &mut (impl Stream<Item = Result<Frame, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<ene_rpc::audio_frame::Frame> {
    timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await.transpose().unwrap()? {
                Frame::Binary(bytes) => {
                    return Some(
                        ene_rpc::AudioFrame::decode_from_slice(&bytes)
                            .unwrap()
                            .frame
                            .unwrap(),
                    );
                }
                Frame::Ping(_) | Frame::Pong(_) => {}
                Frame::Close(_) => return None,
                frame => panic!("unexpected audio frame: {frame:?}"),
            }
        }
    })
    .await
    .unwrap()
}
