use super::*;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use buffa::Message;
use ene_api_types::rpc as w;
use serde_json::json;

#[test]
fn endpoints_keys_and_diagnostics_reject_unsafe_connections() {
    for endpoint in [
        "http://remote/rpc",
        "http://localhost/rpc",
        "http://127.0.0.1/rpc?token=secret",
        "https://user@remote/rpc",
        "https://remote/rpc#secret",
        "file:///rpc",
        "http://127.0.0.1/wrong",
        "http://127.0.0.1/v1",
    ] {
        assert!(matches!(
            ApiClient::new(&RuntimeConnection {
                endpoint: endpoint.into(),
                token: "credential".into()
            }),
            Err(Error::InvalidConnection)
        ));
    }
    for token in ["", "secret\r\nother", "not a token", "日本語"] {
        assert!(
            ApiClient::new(&RuntimeConnection {
                endpoint: "http://127.0.0.1:1234/rpc".into(),
                token: token.into()
            })
            .is_err()
        );
    }
    for endpoint in [
        "http://127.0.0.1:1234/rpc",
        "http://[::1]:1234/rpc",
        "https://remote.example/rpc",
    ] {
        let client = ApiClient::new(&RuntimeConnection {
            endpoint: endpoint.into(),
            token: "credential".into(),
        })
        .unwrap();
        assert!(!format!("{client:?}").contains("credential"));
        assert!(!format!("{client:?}").contains(endpoint));
    }
    assert!(IdempotencyKey::new("x".repeat(200)).is_ok());
    for key in ["".into(), "x".repeat(201), "key\nheader".into()] {
        assert!(IdempotencyKey::new(key).is_err());
    }
}

async fn serve(router: Router) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/rpc", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (
        ApiClient::new(&RuntimeConnection {
            endpoint,
            token: "credential".into(),
        })
        .unwrap(),
        server,
    )
}

#[tokio::test]
async fn auth_redirects_and_response_bounds_are_enforced() {
    use axum::response::IntoResponse;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let reached = Arc::new(AtomicBool::new(false));
    let destination = reached.clone();
    let router = Router::new()
        .route(
            "/rpc/ene.host.v1.HostService/GetSettings",
            post(|headers: HeaderMap| async move {
                assert_eq!(headers[AUTHORIZATION], "Bearer credential");
                (StatusCode::TEMPORARY_REDIRECT, [("location", "/sink")]).into_response()
            }),
        )
        .route(
            "/sink",
            post(move || async move {
                destination.store(true, Ordering::SeqCst);
                Json(json!({}))
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/ListProviders",
            post(|| async { (StatusCode::UNAUTHORIZED, Json(json!({}))) }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/ListCompanions",
            post(|| async {
                // A chunked response cannot evade the actual decoded-byte limit.
                let chunks = futures_util::stream::iter(
                    (0..17).map(|_| Ok::<_, std::convert::Infallible>(vec![b' '; 1024 * 1024])),
                );
                axum::response::Response::builder()
                    .header("content-type", "application/proto")
                    .body(axum::body::Body::from_stream(chunks))
                    .unwrap()
            }),
        );
    let (client, server) = serve(router).await;
    assert!(matches!(
        client.settings().await,
        Err(Error::Protocol | Error::Transport | Error::ResourceLimit)
    ));
    assert!(!reached.load(Ordering::SeqCst));
    assert!(matches!(client.providers().await, Err(Error::Unauthorized)));
    assert!(matches!(
        client.companions().await,
        Err(Error::Protocol | Error::Transport | Error::ResourceLimit)
    ));
    server.abort();
}

#[tokio::test]
async fn opaque_ids_and_cursors_cannot_inject_paths_queries_or_credentials() {
    let router = Router::new().route(
        "/rpc/ene.host.v1.HostService/GetHistory",
        post(|headers: HeaderMap, body: axum::body::Bytes| async move {
            let req = w::HistoryRequest::decode_from_slice(&body).unwrap();
            assert_eq!(req.companion_id, "日本/id?token=evil");
            assert_eq!(req.cursor.as_deref(), Some("cursor&token=evil#日本語"));
            assert_eq!(req.limit, Some(50));
            assert_eq!(headers[AUTHORIZATION], "Bearer credential");
            axum::response::Response::builder()
                .header("content-type", "application/proto")
                .body(axum::body::Body::from(
                    w::History::default().encode_to_vec(),
                ))
                .unwrap()
        }),
    );
    let (client, server) = serve(router).await;
    assert!(
        client
            .messages(
                "日本/id?token=evil",
                Some("cursor&token=evil#日本語"),
                Some(50)
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    server.abort();
}
