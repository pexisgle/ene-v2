use crate::store::{self, Store};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use ene_protocol::host::{Companion, Cursor, Message, Notification, Page, Settings, Snapshot};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, broadcast, watch};
use uuid::Uuid;

pub struct EventBoundary {
    pub cursor: Cursor,
    pub connections: HashSet<String>,
}

#[derive(Clone)]
pub struct App {
    token: Arc<str>,
    pub store: Store,
    pub boundary: Arc<Mutex<EventBoundary>>,
    pub events: broadcast::Sender<Notification>,
    pub shutdown: watch::Sender<bool>,
}

impl App {
    pub fn new(store: Store, token: String) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            token: token.into(),
            store,
            events,
            shutdown: watch::channel(false).0,
            boundary: Arc::new(Mutex::new(EventBoundary {
                cursor: Cursor {
                    epoch: Uuid::new_v4().to_string(),
                    seq: 0,
                },
                connections: HashSet::new(),
            })),
        }
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/companions", get(companions))
        .route("/v1/companions/{companion_id}", get(companion))
        .route("/v1/companions/{companion_id}/messages", get(messages))
        .route(
            "/v1/companions/{companion_id}/messages/{message_id}",
            get(message),
        )
        .route("/v1/settings", get(settings))
        .route("/v1/grants", get(grants))
        .route("/v1/events", get(events))
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "未対応のAPIです")
        })
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .with_state(app)
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }
    fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "ホストの保存処理を利用できません",
        )
    }
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "対象が見つかりません")
    }
    fn invalid() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "要求の形式が正しくありません",
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status,Json(json!({"error":{"code":self.code,"message":self.message,"details":{}},"request_id":Uuid::new_v4().to_string()}))).into_response()
    }
}

async fn authenticate(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    // Fixed-length comparison avoids early exits depending on token contents.
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let allowed = provided.is_some_and(|value| {
        let actual = value.as_bytes();
        let expected = app.token.as_bytes();
        actual.len() == expected.len()
            && actual
                .iter()
                .zip(expected)
                .fold(0u8, |d, (a, b)| d | (a ^ b))
                == 0
    });
    if !allowed {
        return ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "認証が必要です")
            .into_response();
    }
    next.run(request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    connection_id: Option<String>,
}
async fn snapshot(
    State(app): State<App>,
    query: Result<Query<SnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Snapshot>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let boundary = app.boundary.lock().await;
    if query
        .connection_id
        .as_ref()
        .is_some_and(|c| !boundary.connections.contains(c))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "resync_required",
            "通知購読からやり直してください",
        ));
    }
    let stored = app
        .store
        .snapshot()
        .await
        .map_err(|_| ApiError::unavailable())?;
    let message_cursors = BTreeMap::from([(stored.companion.id.clone(), stored.next_cursor)]);
    Ok(Json(Snapshot {
        host_id: stored.host_id,
        environment_id: stored.environment_id,
        capabilities: json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications"],"adapters":[],"model_reference_formats":[]}),
        cursor: boundary.cursor.clone(),
        companions: vec![stored.companion],
        messages: stored.messages,
        message_cursors,
        tasks: vec![],
        questions: vec![],
        settings: stored.settings,
        grants: vec![],
    }))
}
async fn companions(State(app): State<App>) -> Result<Json<Vec<Companion>>, ApiError> {
    Ok(Json(vec![
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .companion,
    ]))
}
async fn companion(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Companion>, ApiError> {
    let c = app
        .store
        .snapshot()
        .await
        .map_err(|_| ApiError::unavailable())?
        .companion;
    if c.id != id {
        return Err(ApiError::not_found());
    }
    Ok(Json(c))
}
async fn settings(State(app): State<App>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(
        app.store
            .snapshot()
            .await
            .map_err(|_| ApiError::unavailable())?
            .settings,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantQuery {
    companion_id: Option<String>,
}
async fn grants(
    State(app): State<App>,
    query: Result<Query<GrantQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    if let Some(c) = query.companion_id {
        let _ = companion(State(app), Path(c)).await?;
    }
    Ok(Json(vec![]))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}
async fn messages(
    State(app): State<App>,
    Path(c): Path<String>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Page<Message>>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let _ = companion(State(app.clone()), Path(c.clone())).await?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit)
        || query
            .cursor
            .as_ref()
            .is_some_and(|cursor| store::parse_cursor(&c, cursor).is_err())
    {
        return Err(ApiError::invalid());
    }
    Ok(Json(
        app.store
            .messages(c, query.cursor, limit)
            .await
            .map_err(|_| ApiError::unavailable())?,
    ))
}
async fn message(
    State(app): State<App>,
    Path((c, id)): Path<(String, String)>,
) -> Result<Json<Message>, ApiError> {
    Ok(Json(
        app.store
            .message(c, id)
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::not_found)?,
    ))
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
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(WsMessage::Close(None))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    async fn app() -> (App, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
        (App::new(store, "private-token".into()), temp)
    }
    async fn request(app: &App, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, token);
        }
        let response = router(app.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn all_endpoints_require_bearer_without_echoing_secrets() {
        let (app, _temp) = app().await;
        for path in ["/v1/snapshot", "/v1/settings", "/v1/events", "/missing"] {
            for token in [
                None,
                Some("Bearer wrong"),
                Some("private-token"),
                Some("Bearer private-token "),
            ] {
                let (status, body) = request(&app, path, token).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(body["error"]["code"], "unauthorized");
                assert!(!body.to_string().contains("private-token"));
            }
        }
    }
    #[tokio::test]
    async fn snapshot_subscription_validation_and_truthful_capabilities() {
        let (app, _temp) = app().await;
        let (status, body) = request(&app, "/v1/snapshot", Some("Bearer private-token")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["capabilities"]["features"],
            json!(["history", "notifications"])
        );
        assert_eq!(
            body["companions"][0]["availability"]["chat"]["ready"],
            false
        );
        assert!(
            body["settings"]["connections"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let (status, _) = request(
            &app,
            "/v1/snapshot?connection_id=missing",
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        app.boundary.lock().await.connections.insert("live".into());
        let (status, _) = request(
            &app,
            "/v1/snapshot?connection_id=live",
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        app.boundary.lock().await.connections.remove("live");
        let (status, _) = request(
            &app,
            "/v1/snapshot?connection_id=live",
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    #[tokio::test]
    async fn history_rejects_foreign_ids_bad_cursors_and_unknown_query_fields() {
        let (app, _temp) = app().await;
        let c = app.store.snapshot().await.unwrap().companion.id;
        for suffix in [
            "limit=0",
            "limit=201",
            "limit=oops",
            "cursor=1:other",
            "cursor=oops",
            "unexpected=1",
        ] {
            let (status, _) = request(
                &app,
                &format!("/v1/companions/{c}/messages?{suffix}"),
                Some("Bearer private-token"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, body) = request(
            &app,
            &format!("/v1/companions/{c}/messages"),
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"], json!([]));
        let (status, _) = request(
            &app,
            "/v1/companions/other/messages",
            Some("Bearer private-token"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
