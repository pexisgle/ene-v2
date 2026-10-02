use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message as Frame, client::IntoClientRequest, protocol::WebSocketConfig},
};

pub enum ServerEvent {
    Notification(Notification),
    ResyncRequired,
}
/// Bound to one live subscription. Do not reuse its ID after closing/reconnecting.
pub struct EventStream {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ready: EventReady,
}
impl ApiClient {
    pub async fn subscribe(&self) -> Result<EventStream, Error> {
        let mut url = self.url(&["events"])?;
        url.set_scheme(if self.0.endpoint.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| Error::InvalidConnection)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::InvalidConnection)?;
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.0.bearer.clone());
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_RESPONSE))
            .max_frame_size(Some(MAX_RESPONSE));
        let (mut socket, _) = timeout(Duration::from_secs(5), connect_async_with_config(request, Some(config), false))
            .await.map_err(|_| Error::Transport)?.map_err(|error| {
                if matches!(error, tokio_tungstenite::tungstenite::Error::Http(ref response) if response.status() == StatusCode::UNAUTHORIZED) {
                    Error::Unauthorized
                } else { Error::Transport }
            })?;
        let ready = timeout(Duration::from_secs(5), async {
            loop {
                match socket.next().await {
                    Some(Ok(Frame::Text(text))) => {
                        let ready: EventReady =
                            serde_json::from_str(&text).map_err(|_| Error::Protocol)?;
                        if ready.kind != "ready" || ready.connection_id.is_empty() {
                            return Err(Error::Protocol);
                        }
                        return Ok(ready);
                    }
                    Some(Ok(Frame::Ping(_))) => {
                        socket.flush().await.map_err(|_| Error::Transport)?
                    }
                    Some(Ok(Frame::Pong(_))) => {}
                    _ => return Err(Error::Protocol),
                }
            }
        })
        .await
        .map_err(|_| Error::Transport)??;
        Ok(EventStream { socket, ready })
    }
}
impl EventStream {
    pub fn ready(&self) -> &EventReady {
        &self.ready
    }
    pub async fn next(&mut self) -> Result<Option<ServerEvent>, Error> {
        loop {
            match self.socket.next().await {
                Some(Ok(Frame::Text(text))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(|_| Error::Protocol)?;
                    if value["type"] == "resync_required" {
                        return Ok(Some(ServerEvent::ResyncRequired));
                    }
                    return serde_json::from_value(value)
                        .map(|v| Some(ServerEvent::Notification(v)))
                        .map_err(|_| Error::Protocol);
                }
                Some(Ok(Frame::Ping(_))) => {
                    timeout(Duration::from_secs(5), self.socket.flush())
                        .await
                        .map_err(|_| Error::Transport)?
                        .map_err(|_| Error::Transport)?;
                }
                Some(Ok(Frame::Pong(_))) => {}
                Some(Ok(Frame::Close(_))) => {
                    let _ = timeout(Duration::from_secs(1), self.socket.flush()).await;
                    return Ok(None);
                }
                None => return Ok(None),
                _ => return Err(Error::Transport),
            }
        }
    }
    /// Send Close and complete the peer handshake within a bounded deadline.
    pub async fn close(mut self) -> Result<(), Error> {
        timeout(Duration::from_secs(1), async {
            self.socket
                .close(None)
                .await
                .map_err(|_| Error::Transport)?;
            while let Some(frame) = self.socket.next().await {
                match frame {
                    Ok(Frame::Close(_)) => return Ok(()),
                    Ok(_) => {}
                    Err(_) => return Err(Error::Transport),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::Transport)?
    }
}
