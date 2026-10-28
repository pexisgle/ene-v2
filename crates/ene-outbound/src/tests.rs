use super::*;
use axum::{
    Router,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::get,
};
use ene_core::{
    domain::{Connection, OperationKind},
    ports::{
        Secret,
        llm::{Authorization, test_support::InvocationFixture},
    },
};
use futures_util::{StreamExt, future::BoxFuture};
use rig_http::http_client::HttpClientExt;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

struct Gate(AtomicBool);
impl Authorization for Gate {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::PermissionRequired))
            }
        })
    }
}
fn transport(
    endpoint: String,
    gate: Arc<Gate>,
    operation: OperationKind,
    placement: CredentialPlacement,
) -> AuthorizedTransport {
    let origin = url::Url::parse(&endpoint)
        .unwrap()
        .origin()
        .ascii_serialization();
    let (_, cancel) = tokio::sync::watch::channel(false);
    let invocation = InvocationFixture {
        id: "test".into(),
        connection: Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: "test".into(),
            endpoint,
            options: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            credential_state: "stored".into(),
            capabilities: Default::default(),
            last_check: Default::default(),
            ..Default::default()
        },
        request: None,
        secret: Some(Secret {
            value: "hidden-secret".into(),
            nonce: "n".into(),
        }),
        authorization: gate,
        cancel,
        operation,
    }
    .with_destinations(vec![origin]);
    AuthorizedTransport::new(Arc::new(invocation), placement).unwrap()
}
async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, task)
}
fn request(url: String) -> http::Request<Bytes> {
    http::Request::builder()
        .uri(url)
        .body(Bytes::new())
        .unwrap()
}

#[tokio::test]
async fn redirects_never_forward_credentials_and_query_failures_are_redacted() {
    let reached = Arc::new(AtomicBool::new(false));
    let sink = reached.clone();
    let (origin, task) = server(
        Router::new()
            .route(
                "/api/models",
                get(|uri: Uri, headers: HeaderMap| async move {
                    assert!(headers.get("authorization").is_none());
                    assert_eq!(
                        url::form_urlencoded::parse(uri.query().unwrap().as_bytes())
                            .next()
                            .unwrap()
                            .1,
                        "hidden-secret"
                    );
                    (StatusCode::TEMPORARY_REDIRECT, [("location", "/sink")]).into_response()
                }),
            )
            .route(
                "/sink",
                get(move || async move {
                    sink.store(true, Ordering::SeqCst);
                    "leaked"
                }),
            ),
    )
    .await;
    let gate = Arc::new(Gate(AtomicBool::new(true)));
    let transport = transport(
        format!("{origin}/api"),
        gate,
        OperationKind::ModelList,
        CredentialPlacement::Query("key".into()),
    );
    let error = transport
        .send(request(format!("{origin}/api/models")))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::PermissionRequired);
    assert!(!format!("{error:?}").contains("hidden-secret"));
    assert!(!reached.load(Ordering::SeqCst));
    let error = transport
        .send(request(format!("{origin}/api/models?key=injected")))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::PermissionRequired);
    task.abort();
}

#[tokio::test]
async fn lazy_send_and_body_receiving_recheck_authority() {
    let arrived = Arc::new(AtomicBool::new(false));
    let saw = arrived.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let body_release = release.clone();
    let (origin, task) = server(Router::new().route(
        "/api/models",
        get(move |headers: HeaderMap| {
            let saw = saw.clone();
            let release = body_release.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer hidden-secret");
                saw.store(true, Ordering::SeqCst);
                axum::body::Body::from_stream(async_stream::stream! {
                    release.notified().await;
                    yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"body"));
                })
            }
        }),
    ))
    .await;
    let gate = Arc::new(Gate(AtomicBool::new(true)));
    let transport = transport(
        format!("{origin}/api"),
        gate.clone(),
        OperationKind::ModelList,
        CredentialPlacement::Bearer,
    );
    let lazy = HttpClientExt::send::<_, Bytes>(&transport, request(format!("{origin}/api/models")));
    gate.0.store(false, Ordering::SeqCst);
    assert!(lazy.await.is_err());
    assert!(!arrived.load(Ordering::SeqCst));
    gate.0.store(true, Ordering::SeqCst);
    let mut response = transport
        .send(request(format!("{origin}/api/models")))
        .await
        .unwrap();
    let reading = response.chunk();
    tokio::pin!(reading);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut reading)
            .await
            .is_err()
    );
    gate.0.store(false, Ordering::SeqCst);
    release.notify_one();
    assert_eq!(
        reading.await.unwrap_err().kind,
        ErrorKind::PermissionRequired
    );
    task.abort();
}

#[tokio::test]
async fn chunked_wire_and_sse_budgets_apply_before_adapter_parsing() {
    let (origin, task) = server(
        Router::new()
            .route(
                "/api/models",
                get(|| async {
                    axum::body::Body::from_stream(futures_util::stream::iter((0..9).map(|_| {
                        Ok::<_, std::convert::Infallible>(Bytes::from(vec![b'x'; 1024 * 1024]))
                    })))
                }),
            )
            .route(
                "/api/stream",
                get(|| async {
                    axum::body::Body::from_stream(futures_util::stream::iter((0..17).map(|_| {
                        Ok::<_, std::convert::Infallible>(Bytes::from(vec![b'x'; 64 * 1024]))
                    })))
                }),
            ),
    )
    .await;
    let transport = transport(
        format!("{origin}/api"),
        Arc::new(Gate(AtomicBool::new(true))),
        OperationKind::Conversation,
        CredentialPlacement::Bearer,
    );
    assert_eq!(
        transport
            .send(request(format!("{origin}/api/models")))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err()
            .kind,
        ErrorKind::ProviderFailure
    );
    let response = transport
        .send_streaming(request(format!("{origin}/api/stream")))
        .await
        .unwrap();
    let mut body = response.into_body();
    let mut failed = false;
    while let Some(chunk) = body.next().await {
        if chunk.is_err() {
            failed = true;
            break;
        }
    }
    assert!(
        failed,
        "oversized unterminated SSE event reached the parser"
    );
    task.abort();
}
#[test]
fn sse_boundaries_survive_chunk_splits() {
    let mut budget = EventBudget::default();
    budget.accept(&vec![b'x'; MAX_EVENT_BYTES - 2]).unwrap();
    budget.accept(b"\n").unwrap();
    budget.accept(b"\n").unwrap();
    budget.accept(&vec![b'x'; MAX_EVENT_BYTES]).unwrap();
    assert!(budget.accept(b"x").is_err());
}
