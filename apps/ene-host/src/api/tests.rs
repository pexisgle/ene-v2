use super::*;
use crate::infrastructure::sqlite::Store;
use axum::http::{StatusCode, header};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;
async fn app() -> (App, tempfile::TempDir, std::sync::Arc<tokio::sync::Notify>) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    let trace = std::sync::Arc::new(tokio::sync::Notify::new());
    (test_application(store, trace.clone()), temp, trace)
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
    let (app, _temp, _trace) = app().await;
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
    let (app, _temp, _trace) = app().await;
    let (status, body) = request(&app, "/v1/snapshot", Some("Bearer private-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["capabilities"]["features"],
        json!([
            "history",
            "notifications",
            "chat",
            "connection_settings",
            "credentials",
            "data_transfer_grants",
            "connection_check",
            "provider_plugins"
        ])
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
    let (app, _temp, _trace) = app().await;
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

async fn mutate(
    app: &App,
    method: &str,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer private-token")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        req = req.header("Idempotency-Key", key);
    }
    let response = router(app.clone())
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}
#[tokio::test]
async fn permissions_are_independent_and_revocation_does_not_wait_for_generation() {
    let (app, _temp, trace) = app().await;
    let (status,saved)=mutate(&app,"PATCH","/v1/settings",json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}),None).await;
    assert_eq!(status, StatusCode::OK);
    let p = saved["connections"][0]["id"].as_str().unwrap();
    let c = app
        .application
        .snapshot(None)
        .await
        .unwrap()
        .stored
        .companion;
    let(status,_)=mutate(&app,"PATCH",&format!("/v1/companions/{}",c.id),json!({"revision":1,"model_assignments":{"companion":{"connection_id":p,"model_id":"gpt-6-luna"}}}),None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mutate(
            &app,
            "PUT",
            &format!("/v1/credentials/{p}"),
            json!({"secret":"test-secret-never-sent"}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let message_path = format!("/v1/companions/{}/messages", c.id);
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"未許可"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let grant = json!({"companion_id":c.id,"connection_id":p,"kind":"data_transfer","destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["test_input"],"purpose":"connection_check"});
    assert_eq!(
        mutate(&app, "POST", "/v1/grants", grant, Some("check-grant"))
            .await
            .0,
        StatusCode::CREATED
    );
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"疎通許可は会話許可を代用しない"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let grant = json!({"companion_id":c.id,"connection_id":p,"kind":"data_transfer","destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"});
    let (status, receipt) = mutate(
        &app,
        "POST",
        "/v1/grants",
        grant.clone(),
        Some("chat-grant"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        mutate(&app, "POST", "/v1/grants", grant, Some("chat-grant"))
            .await
            .1,
        receipt
    );
    let check_id = app
        .application
        .grants(None)
        .await
        .unwrap()
        .iter()
        .find(|g| g.purpose == ene_core::domain::GrantPurpose::ConnectionCheck)
        .unwrap()
        .id
        .clone();
    let (_, acceptance) = mutate(
        &app,
        "POST",
        &message_path,
        json!({"text":"生成中"}),
        Some("accepted"),
    )
    .await;
    let response_id = acceptance["response_message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let check_app = app.clone();
    let check_path = format!("/v1/connections/{p}/check");
    let cid = c.id.clone();
    let checking = tokio::spawn(async move {
        mutate(
            &check_app,
            "POST",
            &check_path,
            json!({"companion_id":cid,"purpose":"companion","model_id":"gpt-6-luna"}),
            Some("running-check"),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), trace.notified())
        .await
        .unwrap();
    let (status, _) = mutate(
        &app,
        "DELETE",
        &format!("/v1/grants/{check_id}"),
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), checking)
            .await
            .unwrap()
            .unwrap()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_ne!(
        app.application
            .message(c.id.clone(), response_id.clone())
            .await
            .unwrap()
            .status,
        ene_core::domain::MessageStatus::Cancelled
    );
    let (status, _) = mutate(
        &app,
        "DELETE",
        &format!("/v1/grants/{}", receipt["grant"]["id"].as_str().unwrap()),
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if app
                .application
                .message(c.id.clone(), response_id.clone())
                .await
                .unwrap()
                .status
                == ene_core::domain::MessageStatus::Cancelled
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        mutate(
            &app,
            "POST",
            &message_path,
            json!({"text":"解除後"}),
            Some("message")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, snapshot) = request(&app, "/v1/snapshot", Some("Bearer private-token")).await;
    assert!(!snapshot.to_string().contains("test-secret-never-sent"));
    assert_eq!(
        snapshot["companions"][0]["availability"]["chat"]["ready"],
        false
    );
}
#[tokio::test]
async fn concurrent_acceptance_replays_one_request_and_never_sends_without_controls() {
    let (app, temp, _trace) = app().await;
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    // Existing durable acceptance must replay even when credentials/grants are absent.
    let c = app
        .application
        .snapshot(None)
        .await
        .unwrap()
        .stored
        .companion
        .id;
    let id = c.clone();
    store
        .run(move |db| {
            crate::infrastructure::sqlite::conversation::accept(
                db,
                "replay",
                &id,
                json!({"text":"日本🙂","resources":[],"question_id":null,"decision":null}),
            )
        })
        .await
        .unwrap();
    let path = format!("/v1/companions/{c}/messages");
    let (a, b) = tokio::join!(
        mutate(
            &app,
            "POST",
            &path,
            json!({"text":"日本🙂"}),
            Some("replay")
        ),
        mutate(
            &app,
            "POST",
            &path,
            json!({"text":"日本🙂"}),
            Some("replay")
        )
    );
    assert_eq!(a.0, StatusCode::ACCEPTED);
    assert_eq!(a, b);
    assert_eq!(
        mutate(&app, "POST", &path, json!({"text":"変更"}), Some("replay"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(store.messages(c, None, 50).await.unwrap().items.len(), 2);
}

fn test_application(store: Store, trace: std::sync::Arc<tokio::sync::Notify>) -> App {
    use std::sync::Arc;
    let shutdown = tokio::sync::watch::channel(false).0;
    App::new(
        ene_core::application::HostApplication::new(
            Arc::new(store),
            Arc::new(TestRegistry(
                crate::infrastructure::llm::registry::Registry::discover(),
                trace,
            )),
            Arc::new(TestCredentials),
            shutdown.clone(),
        ),
        "private-token".into(),
        shutdown,
    )
}
struct TestCredentials;
impl ene_core::ports::Credentials for TestCredentials {
    fn environment(&self, _: &str) -> ene_core::ports::Result<String> {
        Err(ene_core::domain::Error::unavailable())
    }
    fn session_only(&self) -> bool {
        true
    }
    fn load(
        &self,
        _: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<String>> {
        Box::pin(async { Err(ene_core::domain::Error::unavailable()) })
    }
    fn save(
        &self,
        _: String,
        _: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn forget(&self, _: String) {}
}
struct TestRegistry(
    crate::infrastructure::llm::registry::Registry,
    std::sync::Arc<tokio::sync::Notify>,
);
impl ene_core::ports::llm::ProviderRegistry for TestRegistry {
    fn descriptors(&self) -> Vec<ene_core::ports::llm::Descriptor> {
        self.0.descriptors()
    }
    fn policy(
        &self,
        c: &ene_core::domain::Connection,
    ) -> ene_core::ports::Result<ene_core::ports::llm::Policy> {
        self.0.policy(c)
    }
    fn generate(
        &self,
        i: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<
            futures_util::stream::BoxStream<
                'static,
                ene_core::ports::Result<ene_core::ports::llm::Event>,
            >,
        >,
    > {
        Box::pin(async move {
            use futures_util::StreamExt;
            i.authorization.check().await?;
            if i.request.max_tokens == 128 {
                self.1.notify_one();
            }
            Ok(Box::pin(
                futures_util::stream::once(async {
                    Ok(ene_core::ports::llm::Event::Delta("途中".into()))
                })
                .chain(futures_util::stream::pending()),
            ) as _)
        })
    }
    fn models(
        &self,
        i: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<Vec<ene_core::ports::llm::Model>>,
    > {
        self.0.models(i)
    }
    fn reload(&self) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        self.0.reload()
    }
    fn enable(
        &self,
        id: String,
        enabled: bool,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        self.0.enable(id, enabled)
    }
}

#[tokio::test]
async fn openapi_is_authenticated_and_documents_live_routes_and_schemas() {
    let (app, _temp, _trace) = app().await;
    assert_eq!(
        request(&app, "/v1/openapi.json", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, spec) = request(&app, "/v1/openapi.json", Some("Bearer private-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["security"], json!([{"bearerAuth": []}]));
    assert_eq!(
        spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    assert!(spec["paths"].get("/v1/tasks").is_none());
    for (path, item) in spec["paths"].as_object().unwrap() {
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
    assert!(
        spec["components"]["schemas"]["ModelMetadata"]["properties"]
            .get("context_window")
            .is_some()
    );
    assert!(!spec.to_string().contains("private-token"));
}

#[tokio::test]
async fn extraction_and_unknown_methods_keep_the_public_error_envelope() {
    let (app, _temp, _trace) = app().await;
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
    let (status, body) = mutate(&app, "DELETE", "/v1/settings", Value::Null, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

#[tokio::test]
async fn typed_client_interoperates_with_actual_host_and_preserves_grants_replay_and_close() {
    use ene_api_client::{ApiClient, Error, IdempotencyKey};
    use ene_api_types::*;
    let (app, _temp, _trace) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let host = app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router(host)).await.unwrap() });
    let client = ApiClient::new(&RuntimeConnection {
        endpoint: format!("http://{address}/v1"),
        token: "private-token".into(),
    })
    .unwrap();
    let events = client.subscribe().await.unwrap();
    let subscription = events.ready().connection_id.clone();
    let snapshot = client.snapshot(Some(&subscription)).await.unwrap();
    let c = &snapshot.companions[0].id;
    let provider = client.providers().await.unwrap().remove(0);
    let settings = client
        .patch_settings(&SettingsPatch {
            revision: 1,
            connections: vec![ConnectionInput {
                id: None,
                kind: "llm".into(),
                adapter: provider.id,
                endpoint: provider.endpoint,
                options: provider.options,
            }],
        })
        .await
        .unwrap();
    let p = &settings.connections[0].id;
    let credentials = client
        .register_credentials(
            p,
            &CredentialInput {
                secret: Some("never-exposed".into()),
                source: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(credentials.credential_state, "session_only");
    let grant = GrantInput {
        companion_id: c.clone(),
        kind: "data_transfer".into(),
        connection_id: p.clone(),
        destinations: provider.destinations,
        data_kinds: vec!["conversation".into()],
        purpose: "conversation".into(),
    };
    let key = IdempotencyKey::new("typed-grant").unwrap();
    let a = client.create_grant(&grant, &key).await.unwrap();
    let b = client.create_grant(&grant, &key).await.unwrap();
    assert_eq!(a.grant, b.grant);
    assert!(a.questions.is_empty());
    let patched = client
        .patch_grant(
            &a.grant.id,
            &GrantPatch {
                revision: a.grant.revision,
                destinations: None,
                data_kinds: Some(vec!["conversation".into()]),
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.grant.revision, a.grant.revision + 1);
    client
        .patch_companion(
            c,
            &CompanionPatch {
                revision: snapshot.companions[0].revision,
                name: None,
                mind: None,
                model_assignments: Some(
                    [(
                        "companion".into(),
                        Some(ModelAssignment {
                            connection_id: p.clone(),
                            model_id: "gpt-6-luna".into(),
                        }),
                    )]
                    .into(),
                ),
            },
        )
        .await
        .unwrap();
    let input = MessageInput {
        text: "日本🙂".into(),
        resources: vec![],
        question_id: None,
        decision: None,
    };
    let key = IdempotencyKey::new("typed-message").unwrap();
    let accepted = client.send_message(c, &input, &key).await.unwrap();
    let replay = client.send_message(c, &input, &key).await.unwrap();
    assert_eq!(accepted.message_id, replay.message_id);
    assert_eq!(accepted.response_message_id, replay.response_message_id);
    assert_eq!(
        client.message(c, &accepted.message_id).await.unwrap().text,
        "日本🙂"
    );
    assert_eq!(client.messages(c, None, None).await.unwrap().items.len(), 2);
    let err = client
        .patch_settings(&SettingsPatch {
            revision: 1,
            connections: vec![],
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Api(ref failure) if failure.status == 409 && failure.body.error.code == "revision_conflict" && !failure.body.request_id.is_empty())
    );
    client.revoke_grant(&a.grant.id).await.unwrap();
    client.revoke_credentials(p).await.unwrap();
    assert!(
        !serde_json::to_string(&client.snapshot(None).await.unwrap())
            .unwrap()
            .contains("never-exposed")
    );
    events.close().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if client.snapshot(Some(&subscription)).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.abort();
    app.application.stop().await;
}
