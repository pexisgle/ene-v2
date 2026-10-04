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
async fn caller_selected_connection_id_replays_without_duplicate_creation_or_revision_bypass() {
    let (app, _temp) = app().await;
    let id = uuid::Uuid::new_v4().to_string();
    let input = json!({"revision":1,"connections":[{"id":id,"create":true,"kind":"llm",
        "adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]});
    for _ in 0..2 {
        let (status, saved) = mutate(&app, "PATCH", "/v1/settings", input.clone(), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["revision"], 2);
        assert_eq!(saved["connections"].as_array().unwrap().len(), 1);
        assert_eq!(saved["connections"][0]["id"], id);
    }
    let mut conflicting = input.clone();
    conflicting["revision"] = json!(3);
    assert_eq!(
        mutate(&app, "PATCH", "/v1/settings", conflicting.clone(), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    conflicting["revision"] = json!(1);
    conflicting["connections"][0]["id"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        mutate(&app, "PATCH", "/v1/settings", conflicting, None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut unknown = input;
    unknown["revision"] = json!(2);
    unknown["connections"][0]["id"] = json!(uuid::Uuid::new_v4().to_string());
    unknown["connections"][0]["create"] = json!(false);
    assert_eq!(
        mutate(&app, "PATCH", "/v1/settings", unknown, None).await.0,
        StatusCode::NOT_FOUND
    );
}

struct ControlledRegistry {
    registry: crate::infrastructure::llm::registry::Registry,
    started: tokio::sync::mpsc::UnboundedSender<ene_core::ports::llm::Generation>,
    finish: std::sync::Arc<tokio::sync::Semaphore>,
}
impl ene_core::ports::llm::ProviderRegistry for ControlledRegistry {
    fn descriptors(&self) -> Vec<ene_core::ports::llm::Descriptor> {
        self.registry.descriptors()
    }
    fn policy(
        &self,
        c: &ene_core::domain::Connection,
    ) -> ene_core::ports::Result<ene_core::ports::llm::Policy> {
        self.registry.policy(c)
    }
    fn generate(
        &self,
        input: ene_core::ports::llm::Invocation,
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
            input.authorization.check().await?;
            self.started.send(input.request.clone()).unwrap();
            let finish = self.finish.clone();
            Ok(Box::pin(async_stream::stream! {
                yield Ok(ene_core::ports::llm::Event::Delta("途中".into()));
                finish.acquire().await.unwrap().forget();
                yield Ok(ene_core::ports::llm::Event::Finished("stop".into()));
            }) as _)
        })
    }
    fn models(
        &self,
        input: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<Vec<ene_core::ports::llm::Model>>,
    > {
        self.registry.models(input)
    }
    fn reload(&self) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        self.registry.reload()
    }
    fn enable(
        &self,
        id: String,
        enabled: bool,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        self.registry.enable(id, enabled)
    }
}
struct ConversationFixture {
    app: App,
    store: Store,
    companion: String,
    connection: String,
    grant: String,
    started: tokio::sync::mpsc::UnboundedReceiver<ene_core::ports::llm::Generation>,
    finish: std::sync::Arc<tokio::sync::Semaphore>,
    _temp: tempfile::TempDir,
}
#[tokio::test]
async fn stop_waits_for_streaming_and_queued_terminal_writes() {
    let mut f = ConversationFixture::new().await;
    let streaming = f.send("shutdown-streaming").await;
    f.started().await;
    let queued = f.send("shutdown-queued").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let message = f
                .app
                .application
                .message(f.companion.clone(), streaming.clone())
                .await
                .unwrap();
            if message.text == "途中" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Block the real SQLite worker after the streaming delta was persisted.
    // stop must remain pending until terminal updates can reach that worker.
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let blocker = tokio::spawn(async move {
        store
            .run(move |_| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    observed.await.unwrap();
    let stopping = f.app.application.stop();
    tokio::pin!(stopping);
    let premature = tokio::time::timeout(std::time::Duration::from_millis(100), &mut stopping)
        .await
        .is_ok();
    release.send(()).unwrap();
    blocker.await.unwrap();
    assert!(
        !premature,
        "stop returned before cancellation could be persisted"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), stopping)
        .await
        .unwrap()
        .unwrap();
    for id in [streaming.clone(), queued] {
        assert_eq!(
            f.store
                .message(f.companion.clone(), id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ene_core::domain::MessageStatus::Cancelled
        );
    }
    assert_eq!(
        f.store
            .message(f.companion.clone(), streaming)
            .await
            .unwrap()
            .unwrap()
            .text,
        "途中"
    );
    assert!(f.started.try_recv().is_err());
    let rejected = ene_core::application::operations::accept_message(
        f.app.application.clone(),
        f.companion.clone(),
        "after-stop".into(),
        ene_core::application::operations::MessageInput {
            text: "after-stop".into(),
            resources: vec![],
            question_id: None,
            decision: None,
        },
    )
    .await;
    assert_eq!(
        rejected.unwrap_err().kind,
        ene_core::domain::ErrorKind::StateConflict
    );
}
impl ConversationFixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.db")).await.unwrap();
        let companion = store.snapshot().await.unwrap().companions[0].id.clone();
        let (started, receiver) = tokio::sync::mpsc::unbounded_channel();
        let finish = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let shutdown = tokio::sync::watch::channel(false).0;
        let app = App::new(
            ene_core::application::HostApplication::new(
                std::sync::Arc::new(store.clone()),
                std::sync::Arc::new(ControlledRegistry {
                    registry: crate::infrastructure::llm::registry::Registry::discover(),
                    started,
                    finish: finish.clone(),
                }),
                std::sync::Arc::new(TestCredentials),
                shutdown.clone(),
            ),
            "private-token".into(),
            shutdown,
        );
        let (status, settings) = mutate(&app, "PATCH", "/v1/settings", json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}), None).await;
        assert_eq!(status, StatusCode::OK);
        let connection = settings["connections"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(mutate(&app, "PATCH", &format!("/v1/companions/{companion}"), json!({"revision":1,"mind":"old mind","model_assignments":{"companion":{"connection_id":connection,"model_id":"old-model"}}}),None).await.0,StatusCode::OK);
        assert_eq!(
            mutate(
                &app,
                "PUT",
                &format!("/v1/credentials/{connection}"),
                json!({"secret":"fixture"}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        let (status, grant) = mutate(&app,"POST","/v1/grants",json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),Some("grant")).await;
        assert_eq!(status, StatusCode::CREATED);
        Self {
            app,
            store,
            companion,
            connection,
            grant: grant["grant"]["id"].as_str().unwrap().into(),
            started: receiver,
            finish,
            _temp: temp,
        }
    }
    async fn send(&self, key: &str) -> String {
        let (status, accepted) = mutate(
            &self.app,
            "POST",
            &format!("/v1/companions/{}/messages", self.companion),
            json!({"text":key}),
            Some(key),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        accepted["response_message_id"].as_str().unwrap().into()
    }
    async fn started(&mut self) -> ene_core::ports::llm::Generation {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.started.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn terminal(&self, id: &str) -> ene_core::domain::MessageStatus {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = self
                    .app
                    .application
                    .message(self.companion.clone(), id.into())
                    .await
                    .unwrap()
                    .status;
                if !matches!(
                    status,
                    ene_core::domain::MessageStatus::Queued
                        | ene_core::domain::MessageStatus::Streaming
                ) {
                    return status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}
#[tokio::test]
async fn queued_generation_uses_start_time_configuration_and_prior_completed_history() {
    let mut f = ConversationFixture::new().await;
    let first = f.send("first").await;
    let initial = f.started().await;
    assert_eq!(initial.model, "old-model");
    assert_eq!(initial.preamble, "old mind");
    let second = f.send("second").await;
    assert!(f.started.try_recv().is_err());
    assert_eq!(mutate(&f.app,"PATCH",&format!("/v1/companions/{}",f.companion),json!({"revision":2,"mind":"new mind","model_assignments":{"companion":{"connection_id":f.connection,"model_id":"new-model"}}}),None).await.0,StatusCode::OK);
    f.finish.add_permits(1);
    let next = f.started().await;
    assert_eq!(next.model, "new-model");
    assert_eq!(next.preamble, "new mind");
    assert!(
        next.messages
            .iter()
            .any(|m| m.role == "assistant" && m.text == "途中")
    );
    assert_eq!(
        f.terminal(&first).await,
        ene_core::domain::MessageStatus::Completed
    );
    f.finish.add_permits(1);
    assert_eq!(
        f.terminal(&second).await,
        ene_core::domain::MessageStatus::Completed
    );
}
#[tokio::test]
async fn grant_noop_and_expansion_preserve_generation_but_narrowing_cancels_it() {
    let mut f = ConversationFixture::new().await;
    let id = f.send("running").await;
    f.started().await;
    let path = format!("/v1/grants/{}", f.grant);
    for input in [
        json!({"revision":1}),
        json!({"revision":2,"data_kinds":["conversation","metadata"]}),
    ] {
        assert_eq!(
            mutate(&f.app, "PATCH", &path, input, None).await.0,
            StatusCode::OK
        );
        assert_ne!(
            f.app
                .application
                .message(f.companion.clone(), id.clone())
                .await
                .unwrap()
                .status,
            ene_core::domain::MessageStatus::Cancelled
        );
    }
    assert_eq!(
        mutate(
            &f.app,
            "PATCH",
            &path,
            json!({"revision":3,"data_kinds":["metadata"]}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.terminal(&id).await,
        ene_core::domain::MessageStatus::Cancelled
    );
}
#[tokio::test]
async fn terminal_storage_failure_remains_visible_and_blocks_dependent_generation() {
    let mut f = ConversationFixture::new().await;
    let first = f.send("first").await;
    f.started().await;
    let second = f.send("second").await;
    let third = f.send("third").await;
    let fault_id = first.clone();
    f.store.run(move |db| { db.execute_batch(&format!("CREATE TRIGGER fail_terminal BEFORE UPDATE ON messages WHEN NEW.id = '{fault_id}' AND NEW.status IN ('completed','failed','cancelled') BEGIN SELECT RAISE(FAIL,'disk full'); END;"))?; Ok(()) }).await.unwrap();
    let mut subscription = f.app.application.subscribe().await;
    f.finish.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if subscription.receiver.recv().await.unwrap().kind == "generation.persistence_failed" {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.terminal(&second).await,
        ene_core::domain::MessageStatus::Failed
    );
    assert_eq!(
        f.terminal(&third).await,
        ene_core::domain::MessageStatus::Failed
    );
    assert!(f.started.try_recv().is_err());
    let snapshot = f.app.application.snapshot(None).await.unwrap();
    assert_eq!(
        snapshot.stored.companions[0].availability["chat"]["reasons"][0]["code"],
        "storage_error"
    );
    assert_eq!(
        f.app
            .application
            .message(f.companion.clone(), first)
            .await
            .unwrap()
            .status,
        ene_core::domain::MessageStatus::Streaming
    );
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("/v1/companions/{}/messages", f.companion),
            json!({"text":"fourth"}),
            Some("fourth")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        f.app.application.stop().await.unwrap_err().kind,
        ene_core::domain::ErrorKind::StorageFailure
    );
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

struct TestCredentials;

struct RoutedRegistry(tokio::sync::mpsc::UnboundedSender<(String, String)>);
impl ene_core::ports::llm::ProviderRegistry for RoutedRegistry {
    fn descriptors(&self) -> Vec<ene_core::ports::llm::Descriptor> {
        vec![]
    }
    fn policy(
        &self,
        c: &ene_core::domain::Connection,
    ) -> ene_core::ports::Result<ene_core::ports::llm::Policy> {
        Ok(ene_core::ports::llm::Policy {
            fingerprint: json!({"endpoint":c.endpoint,"adapter":c.adapter}),
            destinations: vec![
                url::Url::parse(&c.endpoint)
                    .unwrap()
                    .origin()
                    .ascii_serialization(),
            ],
            requires_credentials: true,
            environment_variable: None,
        })
    }
    fn generate(
        &self,
        input: ene_core::ports::llm::Invocation,
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
            input.authorization.check().await?;
            self.0
                .send((input.connection.endpoint, input.secret.unwrap().value))
                .unwrap();
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                ene_core::ports::llm::Event::Finished("stop".into()),
            )])) as _)
        })
    }
    fn models(
        &self,
        _: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<Vec<ene_core::ports::llm::Model>>,
    > {
        Box::pin(async { Ok(vec![]) })
    }
    fn reload(&self) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn enable(
        &self,
        _: String,
        _: bool,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct StoredCredentials(std::sync::Mutex<std::collections::BTreeMap<String, String>>);
impl ene_core::ports::Credentials for StoredCredentials {
    fn environment(&self, _: &str) -> ene_core::ports::Result<String> {
        Err(ene_core::domain::Error::unavailable())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<String>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .get(&reference)
                .cloned()
                .ok_or_else(ene_core::domain::Error::unavailable)
        })
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().insert(reference, value);
            Ok(())
        })
    }
    fn forget(&self, reference: String) {
        self.0.lock().unwrap().remove(&reference);
    }
}

#[tokio::test]
async fn changing_service_clears_credentials_before_new_authorization() {
    for (adapter, endpoint, destination) in [
        ("service-a", "https://new.test/v1", "https://new.test"),
        ("service-b", "https://old.test/v1", "https://old.test"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.db")).await.unwrap();
        let c = store.snapshot().await.unwrap().companions[0].id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = tokio::sync::watch::channel(false).0;
        let credentials = std::sync::Arc::new(StoredCredentials::default());
        let app = App::new(
            ene_core::application::HostApplication::new(
                std::sync::Arc::new(store.clone()),
                std::sync::Arc::new(RoutedRegistry(tx)),
                credentials.clone(),
                shutdown.clone(),
            ),
            "private-token".into(),
            shutdown,
        );
        let (status, saved) = mutate(
            &app,
            "PATCH",
            "/v1/settings",
            json!({"revision":1,"connections":[
        {"kind":"llm","adapter":"service-a","endpoint":"https://old.test/v1","options":{}}]}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let p = saved["connections"][0]["id"].as_str().unwrap();
        assert_eq!(
            mutate(
                &app,
                "PUT",
                &format!("/v1/credentials/{p}"),
                json!({"secret":"old-service-key"}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(
            ene_core::ports::Repository::configuration(&store)
                .await
                .unwrap()
                .credential_refs
                .contains_key(p)
        );
        assert_eq!(credentials.0.lock().unwrap().len(), 1);
        assert_eq!(mutate(&app,"PATCH",&format!("/v1/companions/{c}"),json!({"revision":1,"model_assignments":{"companion":{"connection_id":p,"model_id":"m"}}}),None).await.0,StatusCode::OK);
        let (status, saved) = mutate(
            &app,
            "PATCH",
            "/v1/settings",
            json!({"revision":2,"connections":[
        {"id":p,"kind":"llm","adapter":adapter,"endpoint":endpoint,"options":{}}]}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["connections"][0]["credential_state"], "missing");
        assert!(
            ene_core::ports::Repository::configuration(&store)
                .await
                .unwrap()
                .credential_refs
                .is_empty()
        );
        assert!(credentials.0.lock().unwrap().is_empty());
        assert_eq!(
            mutate(
                &app,
                "POST",
                "/v1/grants",
                json!({"companion_id":c,"connection_id":p,"kind":"data_transfer",
        "destinations":[destination],"data_kinds":["conversation"],"purpose":"conversation"}),
                Some("new-grant")
            )
            .await
            .0,
            StatusCode::CREATED
        );
        assert_eq!(
            mutate(
                &app,
                "POST",
                &format!("/v1/companions/{c}/messages"),
                json!({"text":"hello"}),
                Some("new-message")
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            mutate(
                &app,
                "PUT",
                &format!("/v1/credentials/{p}"),
                json!({"secret":"new-service-key"}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            mutate(
                &app,
                "POST",
                &format!("/v1/companions/{c}/messages"),
                json!({"text":"hello"}),
                Some("new-message")
            )
            .await
            .0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            (endpoint.into(), "new-service-key".into())
        );
        app.application.stop().await.unwrap();
    }
}

#[tokio::test]
async fn disconnected_revocation_finishes_disk_authority_and_notifications() {
    let f = ConversationFixture::new().await;
    let mut events = f.app.application.subscribe().await;
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let blocked_store = f.store.clone();
    let blocker = tokio::spawn(async move {
        blocked_store
            .run(move |_| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    entered_rx.await.unwrap();
    let mut revoke = Box::pin(
        router(f.app.clone()).oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/grants/{}", f.grant))
                .header(header::AUTHORIZATION, "Bearer private-token")
                .body(Body::empty())
                .unwrap(),
        ),
    );
    assert!(futures_util::poll!(&mut revoke).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let mut grants = Box::pin(f.app.application.grants(None));
            if futures_util::poll!(&mut grants).is_pending() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(revoke);
    release.send(()).unwrap();
    blocker.await.unwrap();
    let persisted = ene_core::ports::Repository::configuration(&f.store)
        .await
        .unwrap();
    assert_eq!(
        persisted.grants[0].status,
        ene_core::domain::GrantStatus::Invalidated
    );
    let effective = f.app.application.grants(None).await.unwrap();
    assert_eq!(
        effective[0].status,
        ene_core::domain::GrantStatus::Invalidated
    );
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("/v1/companions/{}/messages", f.companion),
            json!({"text":"after revocation"}),
            Some("after-revocation")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let notification = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let n = events.receiver.recv().await.unwrap();
            if n.kind == "grants.updated" {
                break n;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(notification.data[0]["status"], "invalidated");
    f.app.finish_mutations().await;
}

#[tokio::test]
async fn long_conversations_keep_recent_turns_within_broker_and_ipc_limits() {
    let mut f = ConversationFixture::new().await;
    for i in 0..17 {
        let text = format!("{i}:{}", "a".repeat(65_000));
        let (status, accepted) = mutate(
            &f.app,
            "POST",
            &format!("/v1/companions/{}/messages", f.companion),
            json!({"text":text}),
            Some(&format!("long-{i}")),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let generation = f.started().await;
        if i == 16 {
            let frame = ene_protocol::provider::HostMessage::Generate {
                request_id: "valid-history".into(),
                request: ene_protocol::provider::GenerationRequest {
                    endpoint: generation.endpoint,
                    options: generation.options,
                    model: generation.model,
                    preamble: generation.preamble,
                    messages: generation
                        .messages
                        .into_iter()
                        .map(|m| ene_protocol::provider::ChatMessage {
                            role: m.role,
                            text: m.text,
                        })
                        .collect(),
                    max_tokens: generation.max_tokens,
                },
            };
            assert!(serde_json::to_vec(&frame).unwrap().len() < 512 * 1024);
            let ene_protocol::provider::HostMessage::Generate { ref request, .. } = frame else {
                unreachable!()
            };
            assert_eq!(request.messages.last().unwrap().text, text);
            assert!(request.messages.len() < 33);
            assert!(!request.messages.iter().any(|m| m.text.starts_with("0:")));
            let mut writer = ene_protocol::ipc::Writer::with_limit(
                tokio::io::sink(),
                ene_protocol::provider::MAX_LINE,
            );
            writer.send(&frame).await.unwrap();
        }
        f.finish.add_permits(1);
        assert_eq!(
            f.terminal(accepted["response_message_id"].as_str().unwrap())
                .await,
            ene_core::domain::MessageStatus::Completed
        );
    }
}

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

#[tokio::test]
async fn revoking_credentials_cancels_a_running_connection_check_without_waiting() {
    let mut f = ConversationFixture::new().await;
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            "/v1/grants",
            json!({
                "companion_id": f.companion,
                "connection_id": f.connection,
                "kind": "data_transfer",
                "destinations": ["https://api.openai.com", "https://models.dev"],
                "data_kinds": ["test_input", "metadata"],
                "purpose": "connection_check"
            }),
            Some("check-grant")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let app = f.app.clone();
    let path = format!("/v1/connections/{}/check", f.connection);
    let input =
        json!({"companion_id": f.companion, "purpose": "companion", "model_id": "check-model"});
    let check =
        tokio::spawn(
            async move { mutate(&app, "POST", &path, input, Some("check-request")).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(1), f.started.recv())
        .await
        .unwrap()
        .unwrap();
    let (status, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        mutate(
            &f.app,
            "DELETE",
            &format!("/v1/credentials/{}", f.connection),
            Value::Null,
            None,
        ),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), check)
            .await
            .unwrap()
            .unwrap()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    f.app.application.stop().await.unwrap();
}

mod credentials;

#[tokio::test]
async fn client_can_reconnect_and_page_escaped_history_without_truncation() {
    let (app, temp) = app().await;
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
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

#[tokio::test]
async fn cancelled_unsent_prompt_is_excluded_from_future_history() {
    let mut f = ConversationFixture::new().await;
    let first = f.send("first-input").await;
    f.started().await;
    let unsent = f.send("withdrawn-sensitive-input").await;
    assert!(f.started.try_recv().is_err());
    assert_eq!(
        mutate(
            &f.app,
            "DELETE",
            &format!("/v1/grants/{}", f.grant),
            Value::Null,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.terminal(&first).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    assert_eq!(
        f.terminal(&unsent).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    assert!(
        f.started.try_recv().is_err(),
        "queued text was never sent to a provider"
    );
    assert_eq!(mutate(&f.app,"POST","/v1/grants",json!({"companion_id":f.companion,"connection_id":f.connection,"kind":"data_transfer","destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),Some("restored-grant")).await.0,StatusCode::CREATED);
    let third = f.send("new-input").await;
    let generation = f.started().await;
    let leaked = generation
        .messages
        .iter()
        .any(|m| m.text == "withdrawn-sensitive-input");
    f.finish.add_permits(1);
    f.terminal(&third).await;
    f.app.application.stop().await.unwrap();
    assert!(
        !leaked,
        "a withdrawn, never-transmitted prompt was automatically added to a later provider request"
    );
}

#[tokio::test]
async fn regrant_does_not_revive_queued_requests() {
    let mut f = ConversationFixture::new().await;
    let first = f.send("running-input").await;
    f.started().await;
    let second = f.send("queued-before-revoke").await;
    let revoke_path = format!("/v1/grants/{}", f.grant);
    let grant = json!({"companion_id":f.companion,"connection_id":f.connection,"kind":"data_transfer","destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"});
    let revoked = mutate(&f.app, "DELETE", &revoke_path, Value::Null, None).await;
    let restored = mutate(
        &f.app,
        "POST",
        "/v1/grants",
        grant,
        Some("replacement-grant"),
    )
    .await;
    assert_eq!(revoked.0, StatusCode::NO_CONTENT);
    assert_eq!(restored.0, StatusCode::CREATED);
    assert_eq!(
        f.terminal(&first).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    let restarted =
        tokio::time::timeout(std::time::Duration::from_millis(200), f.started.recv()).await;
    let was_sent = restarted.is_ok();
    f.finish.add_permits(1);
    assert_eq!(
        f.terminal(&second).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    f.app.application.stop().await.unwrap();
    assert!(
        !was_sent,
        "a request queued before revocation was transmitted after regrant without a new send"
    );
}

#[tokio::test]
async fn revoking_credentials_cancels_queued_conversations() {
    let mut f = ConversationFixture::new().await;
    let first = f.send("running-before-credential-revoke").await;
    f.started().await;
    let second = f.send("queued-before-credential-revoke").await;
    assert_eq!(
        mutate(
            &f.app,
            "DELETE",
            &format!("/v1/credentials/{}", f.connection),
            Value::Null,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.terminal(&first).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    assert_eq!(
        f.terminal(&second).await,
        ene_core::domain::MessageStatus::Cancelled
    );
    assert!(f.started.try_recv().is_err());
    f.app.application.stop().await.unwrap();
}
