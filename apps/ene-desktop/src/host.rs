//! Authenticated local transport. No UI object or provider credential lives here.
use ene_protocol::host::{Message, Notification, Page, Replica, RuntimeConnection, Snapshot};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::mpsc,
    time::Duration,
};
use tokio::sync::mpsc as channel;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as Frame, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MODEL: &str = "openai/gpt-6-luna";
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1";
pub enum Command {
    Send(String),
    Configure,
    Credential(String),
    EnvironmentCredential,
    RevokeCredential,
    Permission { conversation: bool, check: bool },
    Check,
    Companion { name: String, mind: String },
    History,
}
pub enum Event {
    View(Box<Snapshot>, Vec<Message>),
    Status(String, bool),
    Accepted(String),
    SendFailed(String),
    Error(String),
    Done(String),
    Older(Page<Message>),
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

fn descriptor(path: &Path) -> Result<RuntimeConnection, String> {
    // Inspect the opened file rather than a potentially replaced path.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .map_err(|_| "このPCのホストが起動していません")?;
    let meta = file.metadata().map_err(|_| "接続情報を確認できません")?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 8192
    {
        return Err("接続情報の所有者・権限・形式が不正です".into());
    }
    let value: RuntimeConnection =
        serde_json::from_reader(file).map_err(|_| "接続情報を読めません")?;
    let url = url::Url::parse(&value.endpoint).map_err(|_| "接続先が不正です")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/v1"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.token.is_empty()
        || value.token.chars().any(char::is_control)
    {
        return Err("ループバックのホストだけに接続できます".into());
    }
    Ok(value)
}
#[derive(Clone)]
struct Http {
    client: Client,
    connection: std::sync::Arc<RuntimeConnection>,
}
#[derive(Debug)]
enum Error {
    Transport,
    Unauthorized,
    Api(String),
}
impl Http {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> Result<Value, Error> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.connection.endpoint, path))
            .bearer_auth(&self.connection.token)
            .timeout(Duration::from_secs(if path.ends_with("/check") {
                95
            } else {
                5
            }));
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let body: Value = response.json().await.map_err(|_| Error::Transport)?;
        if !status.is_success() {
            return Err(Error::Api(format!(
                "{} ({})",
                body["error"]["message"]
                    .as_str()
                    .unwrap_or("要求を完了できません"),
                body["error"]["code"].as_str().unwrap_or("unavailable")
            )));
        }
        Ok(body)
    }
    async fn snapshot(&self, id: &str) -> Result<Snapshot, Error> {
        serde_json::from_value(
            self.request(
                Method::GET,
                &format!("/snapshot?connection_id={id}"),
                None,
                None,
            )
            .await?,
        )
        .map_err(|_| Error::Transport)
    }
}
fn projection(snapshot: &Snapshot, replica: &Replica) -> Event {
    let mut messages: Vec<_> = replica.messages.values().cloned().collect();
    messages.sort_by_key(|m| m.sequence);
    Event::View(Box::new(snapshot.clone()), messages)
}
struct Pending {
    text: String,
    key: String,
    companion: String,
    host: String,
}
async fn run(commands: channel::UnboundedReceiver<Command>, emit: &(impl Fn(Event) + Sync)) {
    let path = match ene_protocol::paths::runtime() {
        Ok(root) => root.join("ene-host/connection.json"),
        Err(_) => {
            emit(Event::Error("XDG_RUNTIME_DIRを利用できません".into()));
            return;
        }
    };
    run_at(commands, emit, &path).await;
}
async fn run_at(
    mut commands: channel::UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Sync),
    path: &Path,
) {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(95))
        .build()
        .expect("local HTTP client");
    let mut attempt = 0usize;
    let mut pending: Option<Pending> = None;
    loop {
        emit(Event::Status("ホストに接続しています…".into(), false));
        let result = session(
            &client,
            &mut commands,
            emit,
            &mut pending,
            &mut attempt,
            path,
        )
        .await;
        let reason = match result {
            Ok(()) => return,
            Err(reason) => reason,
        };
        let delay = [1, 2, 4, 8, 15][attempt.min(4)];
        attempt = attempt.saturating_add(1);
        emit(Event::Status(
            format!("{reason} · {delay}秒後に再接続します"),
            false,
        ));
        let wait = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {_= &mut wait=>break, command=commands.recv()=>match command {
                None=>return, Some(Command::Send(_))=>emit(Event::SendFailed("未接続です。接続後に送信してください。".into())),Some(_)=>emit(Event::Error("未接続です。接続後に操作してください。".into()))
            }}
        }
    }
}
async fn session(
    client: &Client,
    commands: &mut channel::UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Sync),
    pending: &mut Option<Pending>,
    attempt: &mut usize,
    path: &Path,
) -> Result<(), String> {
    let connection = std::sync::Arc::new(descriptor(path)?);
    let http = Http {
        client: client.clone(),
        connection: connection.clone(),
    };
    let mut request = format!("{}/events", connection.endpoint.replacen("http:", "ws:", 1))
        .into_client_request()
        .map_err(|_| "通知接続が不正です")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", connection.token)
            .parse()
            .map_err(|_| "接続情報が不正です")?,
    );
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(request))
        .await
        .map_err(|_| "通知接続がタイムアウトしました")?
        .map_err(|_| "通知接続を開始できません")?;
    let ready = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .map_err(|_| "購読応答がありません")?
        .ok_or("購読応答がありません")?
        .map_err(|_| "購読を開始できません")?;
    let ready: Value = serde_json::from_str(ready.to_text().map_err(|_| "購読応答が不正です")?)
        .map_err(|_| "購読応答が不正です")?;
    let id = ready["connection_id"]
        .as_str()
        .filter(|_| ready["type"] == "ready")
        .ok_or("購読応答が不正です")?
        .to_owned();
    let (tx, mut events) = channel::channel(256);
    let reader = tokio::spawn(async move {
        while let Some(frame) = socket.next().await {
            match frame {
                Ok(Frame::Text(text)) => {
                    if tx
                        .send(serde_json::from_str::<Value>(&text).ok())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Frame::Ping(_)) => {
                    if socket.flush().await.is_err() {
                        break;
                    }
                }
                Ok(Frame::Pong(_)) => {}
                _ => break,
            }
        }
        // A received Close already queues its reply; flush by closing the sink.
        let _ = SinkExt::close(&mut socket).await;
    });

    let result = async {
        let mut snapshot = http.snapshot(&id).await.map_err(|_| "スナップショットを取得できません")?;
        let mut replica = Replica::from_snapshot(&snapshot);
        *attempt = 0;
        emit(projection(&snapshot, &replica));
        emit(Event::Status("ホストに接続しました".into(), true));
        if pending.as_ref().is_some_and(|p| p.host != snapshot.host_id) {
            pending.take();
            emit(Event::SendFailed("接続先ホストが変わったため、未確認の送信を停止しました".into()));
        }
        let mut jobs = tokio::task::JoinSet::new();
        if let Some(p) = pending.as_ref() {
            spawn_send(&mut jobs, &http, p);
        }
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(Some(value)) = event else { return Err("通知接続が切れました".into()); };
                    if value["type"] == "resync_required" {
                        return Err("通知の再同期が必要です".into());
                    }
                    let notification: Notification = serde_json::from_value(value)
                        .map_err(|_| "通知の形式が不正です")?;
                    if !replica.apply(&notification).map_err(|_| "通知の再同期が必要です")? {
                        continue;
                    }
                    if matches!(notification.kind.as_str(), "settings.updated" | "companion.updated" | "grants.updated") {
                        // Buffer deltas while fetching a snapshot on the live subscription.
                        snapshot = http.snapshot(&id).await.map_err(|_| "設定を同期できません")?;
                        replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                    }
                    emit(projection(&snapshot, &replica));
                },
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    match command {
                        Command::Send(text) => {
                            if pending.is_some() {
                                emit(Event::Error("前の発言の受付を確認しています".into()));
                                continue;
                            }
                            let companion = snapshot.companions.first().ok_or("コンパニオンがありません")?.id.clone();
                            *pending = Some(Pending { text, key: Uuid::new_v4().to_string(), companion, host: snapshot.host_id.clone() });
                            spawn_send(&mut jobs, &http, pending.as_ref().unwrap());
                        }
                        command => {
                            let http = http.clone();
                            let current = snapshot.clone();
                            jobs.spawn(async move { Job::Action(action(http, current, command).await) });
                        }
                    }
                },
                job = jobs.join_next(), if !jobs.is_empty() => {
                    match job {
                        Some(Ok(Job::Sent(result))) => match result {
                            Ok(_) => {
                                if let Some(p) = pending.take() { emit(Event::Accepted(p.text)); }
                            }
                            Err(Error::Transport | Error::Unauthorized) => {
                                return Err("送信の受付を確認できません。同じキーで再確認します".into());
                            }
                            Err(Error::Api(error)) => {
                                pending.take();
                                emit(Event::SendFailed(error));
                            }
                        },
                        Some(Ok(Job::Action(Ok(ActionResult::Done(text))))) => {
                            emit(Event::Done(text));
                            snapshot = http.snapshot(&id).await.map_err(|_| "設定を同期できません")?;
                            replica = refreshed(&http, &snapshot, replica).await.map_err(|_| "履歴を再同期できません")?;
                            emit(projection(&snapshot, &replica));
                        }
                        Some(Ok(Job::Action(Ok(ActionResult::History(page))))) => {
                            for m in &page.items {
                                replica.messages.entry(m.id.clone()).or_insert_with(|| m.clone());
                            }
                            snapshot.message_cursors.insert(snapshot.companions[0].id.clone(), page.next_cursor.clone());
                            emit(projection(&snapshot, &replica));
                            emit(Event::Older(page));
                        }
                        Some(Ok(Job::Action(Err(Error::Api(text))))) => emit(Event::Error(text)),
                        _ => emit(Event::Error("要求を完了できません。接続を確認してください".into())),
                    }
                },
            }
        }
    }.await;
    reader.abort();
    let _ = reader.await;
    result
}
// Final older rows are immutable. A previously active row omitted by the new
// snapshot has ended outside its latest page; obtain that terminal state first.
async fn refreshed(http: &Http, snapshot: &Snapshot, previous: Replica) -> Result<Replica, Error> {
    use ene_protocol::host::MessageStatus;
    let mut fresh = Replica::from_snapshot(snapshot);
    for (key, mut message) in previous.messages {
        if fresh.messages.contains_key(&key) {
            continue;
        }
        if matches!(
            message.status,
            MessageStatus::Queued | MessageStatus::Streaming
        ) {
            let path = format!(
                "/companions/{}/messages/{}",
                message.companion_id, message.id
            );
            message = serde_json::from_value(http.request(Method::GET, &path, None, None).await?)
                .map_err(|_| Error::Transport)?;
            if matches!(
                message.status,
                MessageStatus::Queued | MessageStatus::Streaming
            ) {
                return Err(Error::Transport);
            }
        }
        fresh.messages.insert(key, message);
    }
    Ok(fresh)
}
enum Job {
    Sent(Result<Value, Error>),
    Action(Result<ActionResult, Error>),
}
enum ActionResult {
    Done(String),
    History(Page<Message>),
}
fn spawn_send(jobs: &mut tokio::task::JoinSet<Job>, http: &Http, p: &Pending) {
    let http = http.clone();
    let path = format!("/companions/{}/messages", p.companion);
    let key = p.key.clone();
    let text = p.text.clone();
    jobs.spawn(async move {
        let result = http
            .request(Method::POST, &path, Some(json!({"text":text})), Some(&key))
            .await
            .and_then(|value| {
                serde_json::from_value::<ene_protocol::host::MessageAcceptance>(value.clone())
                    .map_err(|_| Error::Transport)?;
                Ok(value)
            });
        Job::Sent(result)
    });
}
fn connection(snapshot: &Snapshot) -> Result<&Value, Error> {
    snapshot
        .settings
        .connections
        .iter()
        .find(|c| c["adapter"] == "openrouter")
        .ok_or_else(|| Error::Api("先にOpenRouterの接続を設定してください".into()))
}
async fn action(http: Http, snapshot: Snapshot, command: Command) -> Result<ActionResult, Error> {
    let c = snapshot
        .companions
        .first()
        .ok_or_else(|| Error::Api("コンパニオンがありません".into()))?;
    let key = Uuid::new_v4().to_string();
    let mut feedback = "保存しました。";
    match command {
        Command::Configure => {
            let p = connection(&snapshot).ok();
            let mut connections = snapshot.settings.connections.clone();
            if p.is_none() {
                connections.push(json!({"kind":"llm","adapter":"openrouter","endpoint":ENDPOINT,"options":{"only":["openai"],"allow_fallbacks":false}}));
            }
            // Strip server-managed fields when replacing the list.
            let connections:Vec<_>=connections.into_iter().map(|p|json!({"id":p["id"],"kind":p["kind"],"adapter":p["adapter"],"endpoint":p["endpoint"],"options":p["options"]})).collect();
            let saved = http
                .request(
                    Method::PATCH,
                    "/settings",
                    Some(json!({"revision":snapshot.settings.revision,"connections":connections})),
                    None,
                )
                .await?;
            let p = saved["connections"]
                .as_array()
                .and_then(|a| a.iter().find(|p| p["adapter"] == "openrouter"))
                .ok_or(Error::Transport)?;
            http.request(Method::PATCH,&format!("/companions/{}",c.id),Some(json!({"revision":c.revision,"model_assignments":{"companion":{"connection_id":p["id"],"model_id":MODEL},"task":{"connection_id":p["id"],"model_id":MODEL}}})),None).await?;
        }
        Command::Companion { name, mind } => {
            http.request(
                Method::PATCH,
                &format!("/companions/{}", c.id),
                Some(json!({"revision":c.revision,"name":name,"mind":mind})),
                None,
            )
            .await?;
        }
        Command::Permission {
            conversation,
            check,
        } => {
            let p = connection(&snapshot)?;
            let pid = p["id"].as_str().ok_or(Error::Transport)?;
            for (purpose, enabled, data) in [
                ("conversation", conversation, "conversation"),
                ("connection_check", check, "test_input"),
            ] {
                let grants: Vec<_> = snapshot
                    .grants
                    .iter()
                    .filter(|g| {
                        g["companion_id"] == c.id
                            && g["connection_id"] == pid
                            && g["purpose"] == purpose
                            && g["status"] == "active"
                    })
                    .collect();
                if enabled && grants.is_empty() {
                    http.request(Method::POST,"/grants",Some(json!({"companion_id":c.id,"kind":"data_transfer","connection_id":pid,"destinations":["https://openrouter.ai","openai"],"data_kinds":[data],"purpose":purpose})),Some(&format!("{key}-{purpose}"))).await?;
                }
                if !enabled {
                    for g in grants {
                        http.request(
                            Method::DELETE,
                            &format!("/grants/{}", g["id"].as_str().ok_or(Error::Transport)?),
                            None,
                            None,
                        )
                        .await?;
                    }
                }
            }
        }
        Command::History => {
            let Some(cursor) = snapshot.message_cursors.get(&c.id).and_then(|v| v.as_ref()) else {
                return Ok(ActionResult::Done(
                    "過去の履歴はすべて表示しています".into(),
                ));
            };
            let mut url = url::Url::parse(&format!(
                "{}{}",
                http.connection.endpoint,
                format_args!("/companions/{}/messages", c.id)
            ))
            .map_err(|_| Error::Transport)?;
            url.query_pairs_mut().append_pair("cursor", cursor);
            let path = url
                .as_str()
                .strip_prefix(&http.connection.endpoint)
                .ok_or(Error::Transport)?;
            return Ok(ActionResult::History(
                serde_json::from_value(http.request(Method::GET, path, None, None).await?)
                    .map_err(|_| Error::Transport)?,
            ));
        }
        command => {
            let pid = connection(&snapshot)?["id"]
                .as_str()
                .ok_or(Error::Transport)?;
            match command {
                Command::Credential(secret) => {
                    let result = http
                        .request(
                            Method::PUT,
                            &format!("/credentials/{pid}"),
                            Some(json!({"secret":secret})),
                            None,
                        )
                        .await?;
                    feedback = if result["credential_state"] == "stored" {
                        "APIキーを資格情報ストアに保存しました"
                    } else {
                        "APIキーは今回のホスト起動中だけ保持します"
                    };
                }
                Command::EnvironmentCredential => {
                    http.request(
                        Method::PUT,
                        &format!("/credentials/{pid}"),
                        Some(json!({"source":"environment"})),
                        None,
                    )
                    .await?;
                    feedback = "ホストの環境変数からAPIキーを登録しました";
                }
                Command::RevokeCredential => {
                    http.request(Method::DELETE, &format!("/credentials/{pid}"), None, None)
                        .await?;
                    feedback = "資格情報を解除し、生成を停止しました";
                }
                Command::Check => {
                    http.request(
                        Method::POST,
                        &format!("/connections/{pid}/check"),
                        Some(json!({"companion_id":c.id,"purpose":"companion","model_id":MODEL})),
                        Some(&key),
                    )
                    .await?;
                    feedback = "実モデルの逐次応答を確認しました";
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(ActionResult::Done(feedback.into()))
}

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
        final_reply.status = ene_protocol::host::MessageStatus::Completed;
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
        let http = Http {
            client: Client::builder().no_proxy().build().unwrap(),
            connection: std::sync::Arc::new(RuntimeConnection {
                endpoint: format!("http://{address}/v1"),
                token: "private".into(),
            }),
        };
        let mut previous = Replica::from_snapshot(&snapshot);
        previous.messages.insert(old.id.clone(), old);
        let fresh = refreshed(&http, &snapshot, previous).await.unwrap();
        assert_eq!(fresh.messages["old"], final_reply);
        assert_eq!(fresh.cursor.seq, 8);
        server.abort();
    }
}
