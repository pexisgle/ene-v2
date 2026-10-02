use crate::app::App;
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    response::Response,
    routing::get,
};
use futures_util::SinkExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::broadcast;
use uuid::Uuid;

pub(super) fn router() -> Router<App> {
    Router::new().route("/", get(events))
}

async fn events(State(app): State<App>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream(app, socket))
}
async fn send(socket: &mut WebSocket, value: Value) -> bool {
    let Ok(text) = serde_json::to_string(&value) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(WsMessage::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn stream(app: App, mut socket: WebSocket) {
    let mut shutdown = app.shutdown.subscribe();
    if *shutdown.borrow() {
        return;
    }
    let (id, mut notifications, cursor) = {
        let mut boundary = app.boundary.lock().await;
        let id = Uuid::new_v4().to_string();
        let notifications = app.events.subscribe();
        boundary.connections.insert(id.clone());
        (id, notifications, boundary.cursor.clone())
    };
    if send(
        &mut socket,
        json!({"type":"ready","connection_id":id,"cursor":cursor}),
    )
    .await
    {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                incoming=socket.recv()=> {
                    match incoming {
                        Some(Ok(WsMessage::Ping(payload)))=> {
                            if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(WsMessage::Pong(payload))).await,Ok(Ok(()))) {break;}
                        }
                        Some(Ok(WsMessage::Pong(_)))=>{},
                        _=>break,
                    }
                }
                event=notifications.recv()=> {
                    match event {
                        Ok(event)=>if !send(&mut socket,serde_json::to_value(event).unwrap_or(Value::Null)).await {break;},
                        Err(broadcast::error::RecvError::Lagged(_))=> {
                            // Invalidate before asking the client to obtain a fresh snapshot.
                            app.boundary.lock().await.connections.remove(&id);
                            let _=send(&mut socket,json!({"type":"resync_required"})).await;
                            break;
                        }
                        Err(broadcast::error::RecvError::Closed)=>break,
                    }
                }
            }
        }
    }
    app.boundary.lock().await.connections.remove(&id);
    // The peer's Close queues a reply; close the sink to flush it without
    // attempting another send after the protocol has entered the closing state.
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
}
