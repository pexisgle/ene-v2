use super::*;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use buffa::Message;
use ene_rpc as w;
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
async fn update_transport_preserves_connection_presence_and_document_changes() {
    let (settings_tx, mut settings_rx) = tokio::sync::mpsc::unbounded_channel();
    let (companion_tx, mut companion_rx) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new()
        .route(
            "/rpc/ene.host.v1.HostService/UpdateSettings",
            post(move |body: axum::body::Bytes| async move {
                let request = w::UpdateSettingsRequest::decode_from_slice(&body).unwrap();
                settings_tx.send(request).unwrap();
                (
                    [("content-type", "application/proto")],
                    w::Settings::default().encode_to_vec(),
                )
            }),
        )
        .route(
            "/rpc/ene.host.v1.HostService/UpdateCompanion",
            post(move |body: axum::body::Bytes| async move {
                let request = w::UpdateCompanionRequest::decode_from_slice(&body).unwrap();
                companion_tx.send(request.clone()).unwrap();
                (
                    [("content-type", "application/proto")],
                    w::Companion {
                        id: request.id,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }),
        );
    let (client, server) = serve(router).await;
    client
        .patch_settings(&w::UpdateSettingsRequest {
            revision: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!settings_rx.recv().await.unwrap().connections.is_set());
    client
        .patch_settings(&w::UpdateSettingsRequest {
            revision: 1,
            connections: w::ConnectionInputs::default().into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let empty = settings_rx.recv().await.unwrap();
    assert!(empty.connections.is_set());
    assert!(empty.connections.items.is_empty());
    let patch = w::UpdateCompanionRequest {
        revision: 1,
        body: w::JsonDocumentPatch {
            change: Some(w::json_document_patch::Change::Set(Box::new(
                w::JsonDocument {
                    json: r#"{"model":"avatar.vrm"}"#.into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        }
        .into(),
        voice: w::JsonDocumentPatch {
            change: Some(w::json_document_patch::Change::Clear(Box::default())),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    client.patch_companion("c1", &patch).await.unwrap();
    let received = companion_rx.recv().await.unwrap();
    assert_eq!(received.id, "c1");
    assert_eq!(received.body, patch.body);
    assert_eq!(received.voice, patch.voice);
    assert!(!received.companion_assignment.is_set());
    server.abort();
}

#[tokio::test]
async fn auth_redirects_and_connect_default_response_bounds_are_enforced() {
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
                // Use a valid generated message so rejection proves the default limit.
                let payload = w::Companions {
                    items: vec![w::Companion {
                        name: "x".repeat(connectrpc::service::DEFAULT_MAX_MESSAGE_SIZE),
                        ..Default::default()
                    }],
                    ..Default::default()
                }
                .encode_to_vec();
                let chunks = futures_util::stream::iter(
                    payload
                        .chunks(64 * 1024)
                        .map(|chunk| Ok::<_, std::convert::Infallible>(chunk.to_vec()))
                        .collect::<Vec<_>>(),
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
        Err(Error::ResourceLimit)
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
