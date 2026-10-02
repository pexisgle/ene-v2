use super::*;
use crate::infrastructure::sqlite::Store;
use axum::http::{StatusCode, header};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
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
    let subscription = app.application.subscribe().await;
    let (status, _) = request(
        &app,
        &format!("/v1/snapshot?connection_id={}", subscription.id),
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app.application.unsubscribe(&subscription.id).await;
    let (status, _) = request(
        &app,
        &format!("/v1/snapshot?connection_id={}", subscription.id),
        Some("Bearer private-token"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}
#[tokio::test]
async fn history_rejects_foreign_ids_bad_cursors_and_unknown_query_fields() {
    let (app, _temp) = app().await;
    let c = app
        .application
        .snapshot(None)
        .await
        .unwrap()
        .stored
        .companion
        .id;
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
