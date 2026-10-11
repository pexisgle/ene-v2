use super::*;
use ene_core::{
    domain::{Connection, Error, ErrorKind, OperationKind},
    ports::{Result, llm::test_support::InvocationFixture, llm::*},
};
use ene_ipc::{
    provider::{Codec, HttpRequest, HttpResponsePart},
    remoc::rch::mpsc,
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn bundle() -> (tempfile::TempDir, manifest::Bundle) {
    let dir = crate::test_support::package_directory();
    crate::test_support::write(dir.path().join("provider.py"), "").unwrap();
    let value = json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"endpoint":"https://example.com/api","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
    crate::test_support::write(dir.path().join("manifest.json"), value.to_string()).unwrap();
    let b = manifest::load(dir.path()).unwrap();
    (dir, b)
}
struct Permit(AtomicBool);

#[tokio::test]
async fn metadata_authorization_cannot_generate_on_the_credential_origin() {
    use axum::{Json, Router, http::HeaderMap, routing::get};
    use ene_ipc::provider::{AuthProfile, BrokerRouting};
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
        let (_dir, mut bundle) = bundle();
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
            body: Some(br#"{"messages":[{"role":"user","content":"unapproved"}]}"#.to_vec()),
        };
        let (tx, mut rx) = mpsc::channel::<HttpResponsePart, Codec>();
        let draining = tokio::spawn(async move {
            while let Some(part) = rx.recv().await.unwrap() {
                if let HttpResponsePart::Head { status, .. } = part {
                    assert_eq!(status, 200);
                }
            }
        });
        for body in [None, request.body.clone()] {
            let mut post = request.clone();
            post.body = body;
            assert_eq!(
                broker::send(bundle.clone(), input.clone(), post, tx.clone())
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::PermissionRequired
            );
        }
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        let mut catalog = request.clone();
        catalog.method = "GET".into();
        catalog.url = format!("{origin}/api/v1/models?providers=openai");
        catalog.body = None;
        broker::send(bundle.clone(), input.clone(), catalog, tx.clone())
            .await
            .unwrap();
        broker::send(bundle, Arc::new(conversation), request, tx)
            .await
            .unwrap();
        draining.await.unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
#[cfg(unix)]
#[test]
fn bundle_revision_distinguishes_file_boundaries_and_executable_content() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, _) = bundle();
    // Fixed bytes and permissions also guard the persisted fingerprint across
    // SHA-256 library and serde_json feature changes.
    crate::test_support::write(
        dir.path().join("manifest.json"),
        concat!(
            r#"{"auth":{"type":"none"},"command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"#,
            r#""destinations":["https://example.com"],"endpoint":"https://example.com/api","#,
            r#""id":"fixture","name":"Fixture","options":{},"protocol_version":1,"version":"1"}"#,
        ),
    )
    .unwrap();
    let safe = "print('safe')\n#";
    let extra = "\nprint('changed execution')\n";
    crate::test_support::write(dir.path().join("provider.py"), safe).unwrap();
    crate::test_support::write(dir.path().join("z"), extra).unwrap();
    for name in ["manifest.json", "provider.py", "z"] {
        std::fs::set_permissions(
            dir.path().join(name),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let original = manifest::load(dir.path()).unwrap();
    assert_eq!(
        original.revision,
        "6ad8b967fab6d8927db8ab7d1ea7b9dfb3bb9b5ba07116db3d08a7f751e2920d",
    );
    crate::test_support::write(dir.path().join("provider.py"), format!("{safe}z{extra}")).unwrap();
    std::fs::remove_file(dir.path().join("z")).unwrap();
    let changed = manifest::load(dir.path()).unwrap();
    assert_ne!(original.revision, changed.revision);
    assert_ne!(
        original
            .files
            .iter()
            .find(|f| f.path == std::path::Path::new("provider.py"))
            .unwrap()
            .bytes,
        changed
            .files
            .iter()
            .find(|f| f.path == std::path::Path::new("provider.py"))
            .unwrap()
            .bytes
    );
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
                options: json!({}),
                credential_state: "missing".into(),
                capabilities: None,
                last_check: None,
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
    let (_dir, b) = bundle();
    let permit = Arc::new(Permit(AtomicBool::new(false)));
    let (_cancel, input) = invocation(permit.clone());
    let input = Arc::new(input.with_destinations(b.manifest.destinations.clone()));
    let (tx, _rx) = mpsc::channel::<HttpResponsePart, Codec>();
    let request = HttpRequest {
        method: "GET".into(),
        url: "https://example.com/api/models".into(),
        headers: Default::default(),
        body: None,
    };
    assert_eq!(
        broker::send(b.clone(), input.clone(), request.clone(), tx.clone())
            .await
            .unwrap_err()
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
                    body: None
                },
                tx.clone()
            )
            .await
            .unwrap_err()
            .kind,
            ErrorKind::PermissionRequired
        );
    }
}
#[tokio::test]
async fn public_model_metadata_get_is_scoped_and_does_not_forward_credentials() {
    use axum::{Router, http::HeaderMap, routing::get};
    use ene_ipc::provider::AuthProfile;

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
    let (_dir, mut b) = bundle();
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
        body: None,
    };
    let (tx, mut rx) = mpsc::channel::<HttpResponsePart, Codec>();
    assert_eq!(
        broker::send(
            b.clone(),
            Arc::new(conversation),
            request.clone(),
            tx.clone()
        )
        .await
        .unwrap_err()
        .kind,
        ErrorKind::PermissionRequired
    );
    let mut undeclared = b.clone();
    undeclared.manifest.destinations.pop();
    assert_eq!(
        broker::send(undeclared, input.clone(), request.clone(), tx.clone())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::PermissionRequired
    );
    let mut post = request.clone();
    post.method = "POST".into();
    assert_eq!(
        broker::send(b.clone(), input.clone(), post, tx.clone())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::PermissionRequired
    );
    let (result, bytes) = tokio::join!(broker::send(b, input, request, tx), async {
        let mut bytes = Vec::new();
        while let Some(part) = rx.recv().await.unwrap() {
            match part {
                HttpResponsePart::Head { status, .. } => assert_eq!(status, 200),
                HttpResponsePart::Data(data) => bytes.extend(data),
            }
        }
        bytes
    });
    result.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["models"][0]["context_window"],
        4096
    );
    server.abort();
}
