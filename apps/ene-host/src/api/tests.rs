use super::*;
use crate::infrastructure::sqlite::Store;
use axum::http::{StatusCode, header};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use tower::ServiceExt;
async fn app() -> (App, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    (
        crate::bootstrap::application(store, "private-token".into()),
        temp,
    )
}
async fn request(app: &App, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    request_method(app, "GET", path, token).await
}
async fn request_method(
    app: &App,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
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
async fn authentication_guards_reads_mutations_and_unknown_routes() {
    let (app, _temp) = app().await;
    for (method, path) in [
        ("GET", "/v1/openapi.json"),
        ("POST", "/v1/companions/fixture/messages"),
        ("PATCH", "/v1/settings"),
        ("DELETE", "/missing"),
    ] {
        for token in [
            None,
            Some("Bearer wrong"),
            Some("private-token"),
            Some("Bearer private-token "),
        ] {
            let (status, body) = request_method(&app, method, path, token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_eq!(body["error"]["code"], "unauthorized");
            assert!(!body.to_string().contains("private-token"));
        }
    }
    assert_eq!(
        request(&app, "/v1/openapi.json", Some("Bearer private-token"))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn client_can_reconnect_and_page_escaped_history_without_truncation() {
    let (app, temp) = app().await;
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companion.id;
    let target = companion.clone();
    {
        let db =
            tokio_rusqlite::rusqlite::Connection::open(temp.path().join("host.sqlite3")).unwrap();
        for sequence in 1..=50 {
            let message = ene_api_types::Message {
                id: format!("escaped-{sequence}"),
                companion_id: target.clone(),
                role: ene_api_types::Role::Companion,
                text: "\0".repeat(128_000),
                status: ene_api_types::MessageStatus::Completed,
                created_at: "2026-10-01T00:00:00Z".into(),
                sequence,
                resources: vec![],
                in_reply_to: None,
                task_id: None,
                question_id: None,
                failure: None,
            };
            db.execute(
                "INSERT INTO messages VALUES (?1,?2,?3,'completed',?4)",
                tokio_rusqlite::rusqlite::params![
                    message.id,
                    target,
                    sequence as i64,
                    serde_json::to_string(&message).unwrap()
                ],
            )
            .unwrap();
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ene_api_client::ApiClient::new(&ene_api_types::RuntimeConnection {
        endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
        token: "private-token".into(),
    })
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    let snapshot = client.snapshot(None).await.unwrap();
    assert!(snapshot.messages.len() < 50);
    let mut seen: std::collections::BTreeSet<_> =
        snapshot.messages.iter().map(|m| m.sequence).collect();
    let mut cursor = snapshot.message_cursors[&companion].clone();
    assert!(cursor.is_some());
    while let Some(before) = cursor {
        let history = client
            .messages(&companion, Some(&before), Some(100))
            .await
            .unwrap();
        assert!(!history.items.is_empty());
        for message in history.items {
            assert_eq!(message.text, "\0".repeat(128_000));
            assert!(seen.insert(message.sequence));
        }
        cursor = history.next_cursor;
    }
    assert_eq!(seen, (1..=50).collect());
    assert!(
        client.snapshot(None).await.is_ok(),
        "reconnect must remain possible"
    );
    server.abort();
    let _ = server.await;
}
