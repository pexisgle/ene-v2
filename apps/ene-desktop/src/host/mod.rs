//! Authenticated local transport. No UI object or provider credential lives here.
use ene_api_client::{ApiClient as Http, IdempotencyKey, ServerEvent};
use ene_api_types::host::{Message, Page, Replica, RuntimeConnection, Snapshot};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::mpsc,
    time::Duration,
};
use tokio::sync::mpsc as channel;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub enum Command {
    Send(String),
    Configure {
        provider: String,
        endpoint: String,
        model: String,
        options: Value,
    },
    ReloadProviders,
    EnableProvider {
        provider: String,
        enabled: bool,
    },
    Models(String),
    Credential(String, String),
    EnvironmentCredential(String),
    RevokeCredential(String),
    Permission {
        connection: String,
        conversation: bool,
        check: bool,
    },
    Check(String),
    Companion {
        name: String,
        mind: String,
    },
    History,
}
pub enum Event {
    View(Box<Snapshot>, Vec<Message>),
    Status(String, bool),
    Accepted(String),
    SendFailed(String),
    Error(String),
    Done(String, bool),
    Older(Page<Message>),
    Providers(Vec<ene_api_types::provider::ProviderInfo>),
    Models(String, Vec<ene_api_types::provider::Model>),
}
pub struct Controller {
    commands: channel::UnboundedSender<Command>,
    pub events: mpsc::Receiver<Event>,
    stop: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Controller {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (commands, receiver) = channel::unbounded_channel();
        let (sender, events) = mpsc::channel();
        let stop = CancellationToken::new();
        let token = stop.clone();
        let thread = std::thread::spawn(move || {
            let emit = move |event| {
                if sender.send(event).is_ok() {
                    wake();
                }
            };
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(async {
                    tokio::select! { biased; _=token.cancelled()=>{}, _=run(receiver, &emit)=>{} }
                }),
                Err(_) => emit(Event::Error("通信処理を起動できません".into())),
            }
        });
        Self {
            commands,
            events,
            stop,
            thread: Some(thread),
        }
    }
    pub fn request(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

mod session;
mod settings;
mod transport;
use session::run;
#[cfg(test)]
use session::{refreshed, run_at};
use settings::{ActionResult, action};
use transport::{Error, descriptor};
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{State, WebSocketUpgrade},
        http::HeaderMap,
        response::{IntoResponse, Response},
        routing::get,
    };
    use futures_util::SinkExt;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    #[test]
    fn descriptor_rejects_public_symlink_and_remote_endpoints() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connection.json");
        fs::write(
            &path,
            json!({"endpoint":"http://127.0.0.1:1234/v1","token":"private"}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(descriptor(&path).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(descriptor(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(descriptor(&link).is_err());
        for endpoint in [
            "http://remote:1234/v1",
            "https://127.0.0.1:1234/v1",
            "http://127.0.0.1:1234/v1?token=bad",
            "http://user@127.0.0.1:1234/v1",
        ] {
            fs::write(
                &path,
                json!({"endpoint":endpoint,"token":"private"}).to_string(),
            )
            .unwrap();
            assert!(descriptor(&path).is_err());
        }
    }
    #[derive(Clone, Default)]
    struct Fixture {
        keys: Arc<Mutex<Vec<String>>>,
    }
    async fn events(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(|mut socket|async move {
            socket.send(axum::extract::ws::Message::Text(json!({"type":"ready","connection_id":"subscription","cursor":{"epoch":"epoch","seq":0}}).to_string().into())).await.unwrap();
            // This arrives while the client is waiting for the snapshot response.
            socket.send(axum::extract::ws::Message::Text(json!({"epoch":"epoch","seq":1,"type":"message.delta","companion_id":"c","data":{"message_id":"m","offset":0,"delta":"日本🙂"}}).to_string().into())).await.unwrap();
            while let Some(Ok(message))=socket.recv().await {if matches!(message,axum::extract::ws::Message::Close(_)){let _=socket.close().await;break;}}
        })
    }
    async fn snapshot() -> Json<Value> {
        tokio::time::sleep(Duration::from_millis(40)).await;
        Json(
            json!({"host_id":"host","environment_id":"env","capabilities":{},"cursor":{"epoch":"epoch","seq":0},"companions":[{"id":"c","name":"ene","body":null,"voice":null,"mind":"日本語","model_assignments":{},"availability":{"chat":{"ready":true}},"revision":1}],"messages":[{"id":"m","companion_id":"c","role":"companion","text":"","status":"streaming","sequence":1,"created_at":"2026-10-02T00:00:00Z","in_reply_to":null,"task_id":null,"question_id":null,"failure":null}],"message_cursors":{},"tasks":[],"questions":[],"settings":{"revision":1,"connections":[]},"grants":[]}),
        )
    }
    async fn accept(State(f): State<Fixture>, headers: HeaderMap) -> Response {
        let mut keys = f.keys.lock().unwrap();
        keys.push(headers["Idempotency-Key"].to_str().unwrap().into());
        if keys.len() == 1 {
            return "".into_response();
        } // Acceptance persisted, HTTP response lost.
        Json(json!({"request_id":"same-request","message_id":"same-user","response_message_id":"same-response"})).into_response()
    }
    #[tokio::test]
    async fn buffered_unicode_delta_and_uncertain_send_reconnect_with_same_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connection.json");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = Fixture::default();
        let app = Router::new()
            .route("/v1/events", get(events))
            .route("/v1/snapshot", get(snapshot))
            .route("/v1/companions/c/messages", axum::routing::post(accept))
            .with_state(fixture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fs::write(
            &path,
            json!({"endpoint":format!("http://{addr}/v1"),"token":"private"}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let (tx, rx) = channel::unbounded_channel();
        let (out, mut observed) = channel::unbounded_channel();
        let sent = AtomicBool::new(false);
        let emit = |event| {
            if let Event::View(_, messages) = &event
                && messages.iter().any(|m| m.text == "日本🙂")
                && !sent.swap(true, Ordering::SeqCst)
            {
                tx.send(Command::Send("再送確認".into())).unwrap();
            }
            let _ = out.send(event);
        };
        let client = run_at(rx, &emit, &path);
        tokio::pin!(client);
        tokio::time::timeout(Duration::from_secs(8),async {loop {tokio::select!{_= &mut client=>panic!("client stopped before acceptance"),event=observed.recv()=>if matches!(event,Some(Event::Accepted(text)) if text=="再送確認"){break;}}}}).await.unwrap();
        let keys = fixture.keys.lock().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
        server.abort();
    }
}

#[cfg(test)]
mod old_reply_tests {
    use super::*;
    #[tokio::test]
    async fn refresh_obtains_terminal_state_for_older_active_reply_omitted_from_snapshot() {
        let value = json!({"host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"epoch","seq":8},"companions":[],"messages":[],"message_cursors":{},"tasks":[],"questions":[],"settings":{"revision":1,"connections":[]},"grants":[]});
        let snapshot: Snapshot = serde_json::from_value(value).unwrap();
        let old:Message=serde_json::from_value(json!({"id":"old","companion_id":"c","role":"companion","text":"途中","status":"streaming","sequence":1,"created_at":"2026-10-02T00:00:00Z","in_reply_to":null,"task_id":null,"question_id":null,"failure":null})).unwrap();
        let mut final_reply = old.clone();
        final_reply.text = "完了".into();
        final_reply.status = ene_api_types::host::MessageStatus::Completed;
        let json_reply = serde_json::to_value(&final_reply).unwrap();
        let app = axum::Router::new().route(
            "/v1/companions/c/messages/old",
            axum::routing::get(move || {
                let value = json_reply.clone();
                async move { axum::Json(value) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Http::new(&RuntimeConnection {
            endpoint: format!("http://{address}/v1"),
            token: "private".into(),
        })
        .unwrap();
        let mut previous = Replica::from_snapshot(&snapshot);
        previous.messages.insert(old.id.clone(), old);
        let fresh = refreshed(&http, &snapshot, previous).await.unwrap();
        assert_eq!(fresh.messages["old"], final_reply);
        assert_eq!(fresh.cursor.seq, 8);
        server.abort();
    }
}
