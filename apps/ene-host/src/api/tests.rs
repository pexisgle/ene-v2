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

#[tokio::test]
async fn openapi_is_authenticated_and_documents_live_routes_and_schemas() {
    let (app, _temp) = app().await;
    assert_eq!(
        request(&app, "/v1/openapi.json", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, spec) = request(&app, "/v1/openapi.json", Some("Bearer private-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        spec,
        serde_json::to_value(super::openapi::specification()).unwrap()
    );
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["security"], json!([{"bearerAuth": []}]));
    assert_eq!(
        spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    assert!(spec["paths"].get("/v1/tasks").is_none());
    assert!(
        spec["x-ene-events"]["synchronization"]
            .as_str()
            .unwrap()
            .contains("Unicode scalars")
    );
    for (path, item) in spec["paths"].as_object().unwrap() {
        assert!(!path.ends_with('/'), "{path}");
        for (method, operation) in item.as_object().unwrap() {
            if !["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                continue;
            }
            assert!(
                operation["responses"].get("401").is_some(),
                "{method} {path}"
            );
            if method == "get" && !path.contains('{') && path != "/v1/events" {
                assert_eq!(
                    request(&app, path, Some("Bearer private-token")).await.0,
                    StatusCode::OK
                );
            }
        }
    }
    // References in both generated components and the WebSocket extension must resolve.
    fn refs(value: &Value, root: &Value) {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(reference)) = fields.get("$ref") {
                    assert!(
                        root.pointer(reference.strip_prefix('#').unwrap()).is_some(),
                        "{reference}"
                    );
                }
                for value in fields.values() {
                    refs(value, root);
                }
            }
            Value::Array(items) => {
                for item in items {
                    refs(item, root);
                }
            }
            _ => {}
        }
    }
    refs(&spec, &spec);
    assert!(!spec.to_string().contains("private-token"));
}

#[tokio::test]
async fn extraction_and_unknown_methods_keep_the_public_error_envelope() {
    let (app, _temp) = app().await;
    for path in [
        "/v1/events",
        "/v1/companions/%FF",
        "/v1/companions/%FF/messages/%FF",
    ] {
        let (status, body) = request(&app, path, Some("Bearer private-token")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        let error: ene_api_types::ApiErrorBody = serde_json::from_value(body).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(!error.request_id.is_empty());
    }
    let response = router(app)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/settings")
                .header(header::AUTHORIZATION, "Bearer private-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

#[tokio::test]
async fn typed_client_subscribes_snapshots_and_closes_foundation_api() {
    let (app, _temp) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    let client = ene_api_client::ApiClient::new(&ene_api_types::RuntimeConnection {
        endpoint: format!("http://{address}/v1"),
        token: "private-token".into(),
    })
    .unwrap();
    let events = client.subscribe().await.unwrap();
    let snapshot = client
        .snapshot(Some(&events.ready().connection_id))
        .await
        .unwrap();
    assert_eq!(snapshot.companions.len(), 1);
    assert!(snapshot.messages.is_empty());
    let spec = client.openapi().await.unwrap();
    assert!(spec["paths"].get("/v1/providers").is_none());
    assert!(
        spec["paths"]
            .get("/v1/connections/{connection_id}/check")
            .is_none()
    );
    events.close().await.unwrap();
    server.abort();
}
