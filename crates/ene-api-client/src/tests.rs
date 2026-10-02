use super::*;
use axum::{
    Json, Router,
    http::{HeaderMap, Uri},
    routing::get,
};
use serde_json::json;

#[test]
fn endpoints_keys_and_diagnostics_reject_unsafe_connections() {
    for endpoint in [
        "http://remote/v1",
        "http://localhost/v1",
        "http://127.0.0.1/v1?token=secret",
        "https://user@remote/v1",
        "https://remote/v1#secret",
        "file:///v1",
        "http://127.0.0.1/wrong",
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
                endpoint: "http://127.0.0.1:1234/v1".into(),
                token: token.into()
            })
            .is_err()
        );
    }
    for endpoint in [
        "http://127.0.0.1:1234/v1",
        "http://[::1]:1234/v1",
        "https://remote.example/v1",
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
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
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
            "/v1/settings",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers[AUTHORIZATION], "Bearer credential");
                (StatusCode::TEMPORARY_REDIRECT, [("location", "/sink")]).into_response()
            }),
        )
        .route(
            "/sink",
            get(move || async move {
                destination.store(true, Ordering::SeqCst);
                Json(json!({}))
            }),
        )
        .route(
            "/v1/providers",
            get(|| async { (StatusCode::UNAUTHORIZED, Json(json!({}))) }),
        )
        .route(
            "/v1/openapi.json",
            get(|| async {
                // A chunked response cannot evade the actual decoded-byte limit.
                let chunks = futures_util::stream::iter(
                    (0..17).map(|_| Ok::<_, std::convert::Infallible>(vec![b' '; 1024 * 1024])),
                );
                axum::body::Body::from_stream(chunks)
            }),
        );
    let (client, server) = serve(router).await;
    assert!(matches!(client.settings().await, Err(Error::Protocol)));
    assert!(!reached.load(Ordering::SeqCst));
    assert!(matches!(client.providers().await, Err(Error::Unauthorized)));
    assert!(matches!(client.openapi().await, Err(Error::Protocol)));
    server.abort();
}

#[tokio::test]
async fn opaque_ids_and_cursors_cannot_inject_paths_queries_or_credentials() {
    use axum::extract::RawQuery;
    let router = Router::new().fallback(
        |uri: Uri, RawQuery(query): RawQuery, headers: HeaderMap| async move {
            assert_eq!(
                uri.path(),
                "/v1/companions/%E6%97%A5%E6%9C%AC%2Fid%3Ftoken=evil/messages"
            );
            let pairs: Vec<_> = url::form_urlencoded::parse(query.as_ref().unwrap().as_bytes())
                .into_owned()
                .collect();
            assert_eq!(
                pairs,
                [
                    ("cursor".into(), "cursor&token=evil#日本語".into()),
                    ("limit".into(), "50".into())
                ]
            );
            assert_eq!(headers[AUTHORIZATION], "Bearer credential");
            Json(json!({"items":[],"next_cursor":null}))
        },
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
    for id in ["", ".", ".."] {
        assert!(matches!(client.companion(id).await, Err(Error::Protocol)));
    }
    server.abort();
}
