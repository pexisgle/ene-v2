use super as broker;
use super::*;
use base64::Engine;
use ene_core::{
    domain::{Connection, Error, ErrorKind, OperationKind},
    ports::{Result, llm::test_support::InvocationFixture, llm::*},
};
use ene_plugin::HttpRequest;
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn bundle() -> Bundle {
    Bundle {
        name: "Fixture".into(),
        revision: "verified-fixture".into(),
        manifest: serde_json::from_value(json!({"id":"fixture","endpoint":"https://example.com/api","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]})).unwrap(),
    }
}
struct Permit(AtomicBool);

#[tokio::test]
async fn metadata_authorization_cannot_generate_on_the_credential_origin() {
    use axum::{Json, Router, http::HeaderMap, routing::get};
    use ene_plugin::provider::{AuthProfile, BrokerRouting};
    use std::sync::atomic::AtomicUsize;

    for routed in [false, true] {
        let posts = Arc::new(AtomicUsize::new(0));
        let observed = posts.clone();
        let router = Router::new()
            .route(
                "/api/v1/models",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer fixture-secret");
                    Json(json!({"data":[]}))
                }),
            )
            .route(
                "/api/v1/chat/completions",
                axum::routing::post(
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let observed = observed.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer fixture-secret");
                            if routed {
                                assert_eq!(
                                    body["provider"],
                                    json!({"only":["openai"],"allow_fallbacks":false})
                                );
                            }
                            observed.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"ok":true}))
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut bundle = bundle();
        bundle.manifest.destinations = vec![origin.clone()];
        bundle.manifest.auth = AuthProfile::Bearer;
        if routed {
            bundle.manifest.routing = BrokerRouting::OpenRouter {
                only: vec!["openai".into()],
            };
        }
        let (_cancel, mut input) = invocation(Arc::new(Permit(AtomicBool::new(true))));
        input.connection.endpoint = format!("{origin}/api/v1");
        input.request = None;
        input.operation = OperationKind::ModelList;
        input.secret = Some(ene_core::ports::Secret {
            value: "fixture-secret".into(),
            nonce: "n".into(),
        });
        let conversation = InvocationFixture {
            id: input.id.clone(),
            connection: input.connection.clone(),
            request: Some(Generation::for_test(
                input.connection.endpoint.clone(),
                json!({}),
                "model".into(),
                String::new(),
                vec![],
                128,
            )),
            secret: Some(ene_core::ports::Secret {
                value: "fixture-secret".into(),
                nonce: "n".into(),
            }),
            authorization: input.authorization.clone(),
            cancel: input.cancel.clone(),
            operation: OperationKind::Conversation,
        }
        .with_destinations(bundle.manifest.destinations.clone());
        let input = Arc::new(input.with_destinations(bundle.manifest.destinations.clone()));
        let request = HttpRequest {
            method: "POST".into(),
            url: format!("{origin}/api/v1/chat/completions"),
            headers: [("content-type".into(), "application/json".into())].into(),
            body_base64: Some(
                base64::engine::general_purpose::STANDARD
                    .encode(br#"{"messages":[{"role":"user","content":"unapproved"}]}"#),
            ),
        };
        for body in [None, request.body_base64.clone()] {
            let mut post = request.clone();
            post.body_base64 = body;
            assert_eq!(
                broker::send(bundle.clone(), input.clone(), post)
                    .await
                    .err()
                    .unwrap()
                    .kind,
                ErrorKind::PermissionRequired
            );
        }
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        let mut catalog = request.clone();
        catalog.method = "GET".into();
        catalog.url = format!("{origin}/api/v1/models?providers=openai");
        catalog.body_base64 = None;
        broker::send(bundle.clone(), input.clone(), catalog)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        broker::send(bundle, Arc::new(conversation), request)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
impl Authorization for Permit {
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
fn invocation(
    authorization: Arc<dyn Authorization>,
) -> (tokio::sync::watch::Sender<bool>, InvocationFixture) {
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    (
        cancel,
        InvocationFixture {
            id: "generation".into(),
            connection: Connection {
                id: "c".into(),
                kind: "llm".into(),
                adapter: "fixture".into(),
                endpoint: "https://example.com/api".into(),
                options: ene_rpc::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                }
                .into(),
                credential_state: "missing".into(),
                capabilities: Default::default(),
                last_check: Default::default(),
                ..Default::default()
            },
            request: Some(Generation::for_test(
                "https://example.com/api".into(),
                json!({}),
                "model".into(),
                String::new(),
                vec![],
                128,
            )),
            operation: OperationKind::Conversation,
            secret: None,
            authorization,
            cancel: receiver,
        },
    )
}
#[tokio::test]
async fn broker_rejects_revoked_context_other_origins_and_credential_headers() {
    let b = bundle();
    let permit = Arc::new(Permit(AtomicBool::new(false)));
    let (_cancel, input) = invocation(permit.clone());
    let input = Arc::new(input.with_destinations(b.manifest.destinations.clone()));
    let request = HttpRequest {
        method: "GET".into(),
        url: "https://example.com/api/models".into(),
        headers: Default::default(),
        body_base64: None,
    };
    assert_eq!(
        broker::send(b.clone(), input.clone(), request.clone())
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::PermissionRequired
    );
    permit.0.store(true, Ordering::SeqCst);
    for (url, headers) in [
        ("https://other.com/api/models", Default::default()),
        ("https://example.com/elsewhere", Default::default()),
        (
            "https://example.com/api/models",
            [("authorization".into(), "Bearer injected".into())].into(),
        ),
    ] {
        assert_eq!(
            broker::send(
                b.clone(),
                input.clone(),
                HttpRequest {
                    method: request.method.clone(),
                    url: url.into(),
                    headers,
                    body_base64: None
                }
            )
            .await
            .err()
            .unwrap()
            .kind,
            ErrorKind::PermissionRequired
        );
    }
}
#[tokio::test]
async fn public_model_metadata_get_is_scoped_and_does_not_forward_credentials() {
    use axum::{Router, http::HeaderMap, routing::get};
    use ene_plugin::provider::AuthProfile;

    let app = Router::new().route(
        "/catalog",
        get(|headers: HeaderMap| async move {
            assert!(headers.get("authorization").is_none());
            assert!(headers.get("x-api-version").is_none());
            assert!(headers.get("cookie").is_none());
            r#"{"models":[{"id":"local-model","context_window":4096}]}"#
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut b = bundle();
    b.manifest.auth = AuthProfile::Bearer;
    b.manifest
        .headers
        .insert("x-api-version".into(), "example".into());
    b.manifest.destinations.push(origin.clone());
    let (_cancel, mut input) = invocation(Arc::new(Permit(AtomicBool::new(true))));
    input.secret = Some(ene_core::ports::Secret {
        value: "fixture-secret".into(),
        nonce: "fixture".into(),
    });
    let conversation = InvocationFixture {
        id: input.id.clone(),
        connection: input.connection.clone(),
        request: input.request.clone(),
        secret: None,
        authorization: input.authorization.clone(),
        cancel: input.cancel.clone(),
        operation: OperationKind::Conversation,
    }
    .with_destinations(b.manifest.destinations.clone());
    input.operation = OperationKind::ModelList;
    input.request = None;
    let input = Arc::new(input.with_destinations(b.manifest.destinations.clone()));
    let request = HttpRequest {
        method: "GET".into(),
        url: format!("{origin}/catalog"),
        headers: Default::default(),
        body_base64: None,
    };
    assert_eq!(
        broker::send(b.clone(), Arc::new(conversation), request.clone())
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::PermissionRequired
    );
    let mut undeclared = b.clone();
    undeclared.manifest.destinations.pop();
    assert_eq!(
        broker::send(undeclared, input.clone(), request.clone())
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::PermissionRequired
    );
    let mut post = request.clone();
    post.method = "POST".into();
    assert_eq!(
        broker::send(b.clone(), input.clone(), post)
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::PermissionRequired
    );
    let response = broker::send(b, input, request).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let bytes = response.bytes().await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["models"][0]["context_window"],
        4096
    );
    server.abort();
}
