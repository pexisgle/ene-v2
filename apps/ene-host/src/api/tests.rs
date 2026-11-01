#[allow(dead_code)]
#[path = "tests/connect_fixture.rs"]
mod connect_fixture;
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
async fn request_method(
    app: &App,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/proto")
        .header("connect-protocol-version", "1");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, token);
    }
    let response = router(app.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn authentication_guards_reads_mutations_and_unknown_routes() {
    let (app, _temp) = app().await;
    for path in [
        "/rpc/ene.host.v1.HostService/GetSettings",
        "/rpc/ene.host.v1.HostService/SendMessage",
        "/rpc/ene.host.v1.HostService/UpdateSettings",
        "/missing",
    ] {
        for token in [
            None,
            Some("Bearer wrong"),
            Some("private-token"),
            Some("Bearer private-token "),
        ] {
            let (status, body) = request_method(&app, "POST", path, token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert!(!body.to_string().contains("private-token"));
        }
    }
    for path in ["/v1/settings", "/v1/openapi.json"] {
        assert_eq!(
            request_method(&app, "POST", path, Some("Bearer private-token"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    for service in ["HostService/GetSettings", "DeviceService/List"] {
        let path = format!("/rpc/ene.host.v2.{service}");
        let (status, body) =
            request_method(&app, "POST", &path, Some("Bearer private-token")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "unimplemented");
    }
}

#[tokio::test]
async fn caller_selected_connection_id_replays_without_duplicate_creation_or_revision_bypass() {
    let (app, _temp) = app().await;
    let id = uuid::Uuid::new_v4().to_string();
    let input = json!({"revision":1,"connections":[{"id":id,"create":true,"kind":"llm",
        "adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]});
    for _ in 0..2 {
        let (status, saved) = mutate(&app, "PATCH", "Settings", input.clone(), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["revision"], 2);
        assert_eq!(saved["connections"].as_array().unwrap().len(), 1);
        assert_eq!(saved["connections"][0]["id"], id);
    }
    let mut conflicting = input.clone();
    conflicting["revision"] = json!(3);
    assert_eq!(
        mutate(&app, "PATCH", "Settings", conflicting.clone(), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    conflicting["revision"] = json!(1);
    conflicting["connections"][0]["id"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        mutate(&app, "PATCH", "Settings", conflicting, None).await.0,
        StatusCode::CONFLICT
    );
    let mut unknown = input;
    unknown["revision"] = json!(2);
    unknown["connections"][0]["id"] = json!(uuid::Uuid::new_v4().to_string());
    unknown["connections"][0]["create"] = json!(false);
    assert_eq!(
        mutate(&app, "PATCH", "Settings", unknown, None).await.0,
        StatusCode::NOT_FOUND
    );
}

struct ControlledRegistry {
    registry: crate::infrastructure::llm::registry::Registry,
    started: tokio::sync::mpsc::UnboundedSender<crate::llm::Generation>,
    finish: std::sync::Arc<tokio::sync::Semaphore>,
}
struct FixtureProvider {
    policy: crate::llm::Policy,
    started: Option<tokio::sync::mpsc::UnboundedSender<crate::llm::Generation>>,
    finish: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    send_outbound: bool,
}
impl crate::llm::LlmProvider for FixtureProvider {
    fn policy(&self, _: &crate::domain::Connection) -> crate::ports::Result<crate::llm::Policy> {
        Ok(self.policy.clone())
    }
    fn generate(
        &self,
        input: crate::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<
            futures_util::stream::BoxStream<'static, crate::ports::Result<crate::llm::Event>>,
        >,
    > {
        Box::pin(async move {
            input.authorization().check().await?;
            if let Some(started) = &self.started {
                started.send(input.generation()?.clone()).unwrap();
            }
            if self.send_outbound {
                let request = http::Request::builder()
                    .method("POST")
                    .uri(format!("{}/chat/completions", input.connection().endpoint))
                    .body(bytes::Bytes::new())
                    .unwrap();
                crate::llm::AuthorizedTransport::new(
                    std::sync::Arc::new(input),
                    crate::llm::CredentialPlacement::Bearer,
                )?
                .send(request)
                .await?
                .bytes()
                .await?;
            }
            let finish = self.finish.clone();
            Ok(Box::pin(async_stream::stream! {
                if let Some(finish) = finish { yield Ok(crate::llm::Event::Delta("途中".into())); finish.acquire().await.unwrap().forget(); }
                yield Ok(crate::llm::Event::Finished("stop".into()));
            }) as _)
        })
    }
    fn models(
        &self,
        _: crate::llm::Invocation,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<Vec<crate::llm::Model>>> {
        Box::pin(async { Err(crate::domain::Error::unavailable()) })
    }
}
impl crate::llm::ProviderRegistry for ControlledRegistry {
    fn resolve(
        &self,
        c: &crate::domain::Connection,
    ) -> crate::ports::Result<crate::llm::ProviderBinding> {
        crate::llm::ProviderBinding::resolve(
            std::sync::Arc::new(FixtureProvider {
                policy: self.registry.resolve(c)?.policy().clone(),
                started: Some(self.started.clone()),
                finish: Some(self.finish.clone()),
                send_outbound: false,
            }),
            c,
        )
    }
    fn descriptors(&self) -> Vec<crate::llm::Descriptor> {
        self.registry.descriptors()
    }

    fn reload(&self) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.registry.reload()
    }
    fn enable(
        &self,
        id: String,
        enabled: bool,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.registry.enable(id, enabled)
    }
}
struct ConversationFixture {
    app: App,
    store: Store,
    companion: String,
    connection: String,
    grant: String,
    started: tokio::sync::mpsc::UnboundedReceiver<crate::llm::Generation>,
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
            "cancelled"
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
    let rejected = f
        .app
        .application
        .accept_message(
            f.companion.clone(),
            "after-stop".into(),
            crate::application::MessageInput {
                text: "after-stop".into(),
                resources: vec![],
                question_id: None,
                decision: None,
                ..Default::default()
            },
        )
        .await;
    assert_eq!(
        rejected.unwrap_err().kind,
        crate::domain::ErrorKind::Unavailable
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
            crate::application::HostApplication::new(
                std::sync::Arc::new(store.clone()),
                std::sync::Arc::new(ControlledRegistry {
                    registry: crate::infrastructure::llm::registry::Registry::discover(),
                    started,
                    finish: finish.clone(),
                }),
                std::sync::Arc::new(TestCredentials),
                shutdown,
            ),
            "private-token".into(),
        );
        let (status, settings) = mutate(&app, "PATCH", "Settings", json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}), None).await;
        assert_eq!(status, StatusCode::OK);
        let connection = settings["connections"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(mutate(&app, "PATCH", &format!("Companion/{companion}"), json!({"revision":1,"mind":"old mind","model_assignments":{"companion":{"connection_id":connection,"model_id":"old-model"}}}),None).await.0,StatusCode::OK);
        assert_eq!(
            mutate(
                &app,
                "PUT",
                &format!("Credentials/{connection}"),
                json!({"secret":"fixture"}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        let (status, grant) = mutate(&app,"POST","Grants",json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),Some("grant")).await;
        assert_eq!(status, StatusCode::OK);
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
            &format!("SendMessage/{}", self.companion),
            json!({"text":key}),
            Some(key),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        accepted["response_message_id"].as_str().unwrap().into()
    }
    async fn started(&mut self) -> crate::llm::Generation {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.started.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn terminal(&self, id: &str) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = self
                    .app
                    .application
                    .message(self.companion.clone(), id.into())
                    .await
                    .unwrap()
                    .status;
                if !matches!(status.as_str(), "queued" | "streaming") {
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
    assert_eq!(initial.model(), "old-model");
    assert_eq!(initial.preamble(), "old mind");
    let second = f.send("second").await;
    assert!(f.started.try_recv().is_err());
    assert_eq!(mutate(&f.app,"PATCH",&format!("Companion/{}",f.companion),json!({"revision":2,"mind":"new mind","model_assignments":{"companion":{"connection_id":f.connection,"model_id":"new-model"}}}),None).await.0,StatusCode::OK);
    f.finish.add_permits(1);
    let next = f.started().await;
    assert_eq!(next.model(), "new-model");
    assert_eq!(next.preamble(), "new mind");
    assert!(
        next.messages()
            .iter()
            .any(|m| m.role == crate::llm::ChatRole::Assistant && m.text == "途中")
    );
    assert_eq!(f.terminal(&first).await, "completed");
    f.finish.add_permits(1);
    assert_eq!(f.terminal(&second).await, "completed");
}
#[tokio::test]
async fn grant_noop_and_expansion_preserve_generation_but_narrowing_cancels_it() {
    let mut f = ConversationFixture::new().await;
    let id = f.send("running").await;
    f.started().await;
    let path = format!("Grant/{}", f.grant);
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
            "cancelled"
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
    assert_eq!(f.terminal(&id).await, "cancelled");
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
            if matches!(
                subscription.receiver.recv().await.unwrap().event,
                Some(ene_rpc::notification::Event::GenerationPersistenceFailed(_))
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(f.terminal(&second).await, "failed");
    assert_eq!(f.terminal(&third).await, "failed");
    assert!(f.started.try_recv().is_err());
    let snapshot = f.app.application.snapshot(None).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&snapshot.stored.companions[0].availability.json).unwrap()["chat"]
            ["reasons"][0]["code"],
        "storage_error"
    );
    assert_eq!(
        f.app
            .application
            .message(f.companion.clone(), first)
            .await
            .unwrap()
            .status,
        "streaming"
    );
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("SendMessage/{}", f.companion),
            json!({"text":"fourth"}),
            Some("fourth")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        f.app.application.stop().await.unwrap_err().kind,
        crate::domain::ErrorKind::StorageFailure
    );
}

fn operation(
    method: &str,
    descriptor: &str,
    mut body: Value,
    key: Option<&str>,
) -> (String, Vec<u8>) {
    if body.is_null() {
        body = json!({});
    }
    let (kind, id) = descriptor.split_once('/').unwrap_or((descriptor, ""));
    let op = match (method, kind) {
        ("PATCH", "Settings") => "UpdateSettings",
        ("PATCH", "Companion") => {
            body["id"] = json!(id);
            "UpdateCompanion"
        }
        ("POST", "SendMessage") => {
            body["companion_id"] = json!(id);
            "SendMessage"
        }
        ("POST", "Grants") => "CreateGrant",
        ("PATCH", "Grant") => {
            body["id"] = json!(id);
            "UpdateGrant"
        }
        ("DELETE", "Grant") => {
            body["id"] = json!(id);
            "RevokeGrant"
        }
        ("PUT", "Credentials") => {
            body["id"] = json!(id);
            "RegisterCredentials"
        }
        ("DELETE", "Credentials") => {
            body["id"] = json!(id);
            "RevokeCredentials"
        }
        ("POST", "CheckConnection") => {
            body["id"] = json!(id);
            "CheckConnection"
        }
        _ => panic!("unmapped fixture {method} {descriptor}"),
    };
    if let Some(key) = key {
        body["request_key"] = json!(key);
    }
    (op.into(), connect_fixture::encode_request(op, body))
}
async fn mutate(
    app: &App,
    method: &str,
    descriptor: &str,
    body: Value,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let (op, bytes) = operation(method, descriptor, body, key);
    let response = router(app.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/rpc/ene.host.v1.HostService/{op}"))
                .header(header::AUTHORIZATION, "Bearer private-token")
                .header(header::CONTENT_TYPE, "application/proto")
                .header("connect-protocol-version", "1")
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    if status.is_success() {
        return (status, connect_fixture::decode_response(&op, &bytes));
    }
    let error: connectrpc::ConnectError = serde_json::from_slice(&bytes).unwrap();
    for detail in error.details {
        if let Some(encoded) = detail.value {
            use base64::Engine;
            use buffa::Message;
            let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(encoded)
                .unwrap();
            let failure = ene_rpc::rpc::ApiFailure::decode_from_slice(&bytes).unwrap();
            return (
                StatusCode::from_u16(failure.http_status as u16).unwrap(),
                connect_fixture::normalized(serde_json::to_value(failure).unwrap()),
            );
        }
    }
    panic!("missing API failure detail for {op}: {status}")
}

struct TestCredentials;

mod grant_consent_tests {
    use super::*;
    use crate::{domain::Connection, llm::*, ports::Result};
    use futures_util::future::BoxFuture;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    struct ReloadableRegistry {
        revision: AtomicU64,
    }
    impl ReloadableRegistry {
        fn policy(&self, c: &Connection) -> Result<Policy> {
            let revision = self.revision.load(Ordering::SeqCst).to_string();
            let mut destinations = vec!["https://old.test".into()];
            if revision != "1" {
                destinations.push("https://undisplayed.test".into());
            }
            Ok(Policy {
                fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":c.options,"implementation":revision}),
                revision,
                destinations,
                requires_credentials: false,
                environment_variable: None,
            })
        }
    }
    impl ProviderRegistry for ReloadableRegistry {
        fn resolve(&self, c: &Connection) -> Result<ProviderBinding> {
            ProviderBinding::resolve(
                Arc::new(FixtureProvider {
                    policy: self.policy(c)?,
                    started: None,
                    finish: None,
                    send_outbound: false,
                }),
                c,
            )
        }
        fn descriptors(&self) -> Vec<Descriptor> {
            vec![]
        }

        fn reload(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async {
                self.revision.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
        fn enable(&self, _: String, _: bool) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn changed_implementation_or_connection_cannot_acquire_undisplayed_consent() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.db")).await.unwrap();
        let companion = store.snapshot().await.unwrap().companions[0].id.clone();
        let shutdown = tokio::sync::watch::channel(false).0;
        let app = App::new(
            crate::application::HostApplication::new(
                Arc::new(store),
                Arc::new(ReloadableRegistry {
                    revision: AtomicU64::new(1),
                }),
                Arc::new(TestCredentials),
                shutdown,
            ),
            "private-token".into(),
        );
        let (status, settings) = mutate(
            &app,
            "PATCH",
            "Settings",
            json!({"revision":1,"connections":[
                {"kind":"llm","adapter":"fixture","endpoint":"https://old.test/v1","options":{}}
            ]}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let connection = settings["connections"][0]["id"].as_str().unwrap();
        let mut input = json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer",
            "destinations":["https://old.test"],"data_kinds":["conversation"],"purpose":"conversation",
            "consent":{"settings_revision":2,"provider_revision":"1"}});
        let mut missing = input.clone();
        missing.as_object_mut().unwrap().remove("consent");
        assert_eq!(
            mutate(&app, "POST", "Grants", missing, Some("missing"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );

        // Reload changes executable identity and adds a recipient after the
        // user saw the old scope. Neither old nor expanded destinations grant it.
        app.application.reload_providers().await.unwrap();
        for destinations in [
            json!(["https://old.test"]),
            json!(["https://old.test", "https://undisplayed.test"]),
        ] {
            input["destinations"] = destinations;
            let (status, error) = mutate(
                &app,
                "POST",
                "Grants",
                input.clone(),
                Some("stale-provider"),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(error["error"]["code"], "revision_conflict");
        }
        assert!(app.application.grants(None).await.unwrap().is_empty());

        input["consent"]["provider_revision"] = json!("2");
        let (status, saved) =
            mutate(&app, "POST", "Grants", input.clone(), Some("confirmed")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            saved["grant"]["connection_fingerprint"]["provider"]["implementation"],
            "2"
        );
        let mut changed_consent = input.clone();
        changed_consent["consent"]["provider_revision"] = json!("1");
        assert_eq!(
            mutate(&app, "POST", "Grants", changed_consent, Some("confirmed"))
                .await
                .0,
            StatusCode::CONFLICT
        );

        // Same adapter and destinations, but connection configuration changed.
        assert_eq!(mutate(&app, "PATCH", "Settings", json!({"revision":2,"connections":[
            {"id":connection,"kind":"llm","adapter":"fixture","endpoint":"https://old.test/v1","options":{"changed":true}}
        ]}), None).await.0, StatusCode::OK);
        let (status, error) = mutate(
            &app,
            "POST",
            "Grants",
            input.clone(),
            Some("stale-settings"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["error"]["code"], "revision_conflict");
        // Receipt replay returns the original result; it must not reauthorize
        // the modified connection with a new policy fingerprint.
        assert_eq!(
            mutate(&app, "POST", "Grants", input.clone(), Some("confirmed"))
                .await
                .1,
            saved
        );
        assert!(
            app.application
                .grants(None)
                .await
                .unwrap()
                .iter()
                .all(|g| g.status != crate::domain::GrantStatus::Active)
        );
        input["consent"]["settings_revision"] = json!(3);
        assert_eq!(
            mutate(&app, "POST", "Grants", input, Some("reconfirmed"))
                .await
                .0,
            StatusCode::OK
        );
        app.application.stop().await.unwrap();
    }
}

struct RoutedRegistry;
impl RoutedRegistry {
    fn policy(&self, c: &crate::domain::Connection) -> crate::ports::Result<crate::llm::Policy> {
        Ok(crate::llm::Policy {
            revision: "routed-v1".into(),
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
}
impl crate::llm::ProviderRegistry for RoutedRegistry {
    fn resolve(
        &self,
        c: &crate::domain::Connection,
    ) -> crate::ports::Result<crate::llm::ProviderBinding> {
        crate::llm::ProviderBinding::resolve(
            std::sync::Arc::new(FixtureProvider {
                policy: self.policy(c)?,
                started: None,
                finish: None,
                send_outbound: true,
            }),
            c,
        )
    }
    fn descriptors(&self) -> Vec<crate::llm::Descriptor> {
        vec![]
    }

    fn reload(&self) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn enable(
        &self,
        _: String,
        _: bool,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct StoredCredentials(std::sync::Mutex<std::collections::BTreeMap<String, String>>);
impl crate::ports::Credentials for StoredCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        Err(crate::domain::Error::unavailable())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .get(&reference)
                .cloned()
                .ok_or_else(crate::domain::Error::unavailable)
        })
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().insert(reference, value);
            Ok(())
        })
    }
    fn forget(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().remove(&reference);
            Ok(())
        })
    }
}

#[tokio::test]
async fn changing_service_clears_credentials_before_new_authorization() {
    for (adapter, path) in [("service-a", "new"), ("service-b", "old")] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = format!("http://{}", listener.local_addr().unwrap());
        let old_endpoint = format!("{destination}/old/v1");
        let endpoint = format!("{destination}/{path}/v1");
        let wire_origin = destination.clone();
        let wire_tx = tx.clone();
        let service = axum::Router::new().route(
            "/{service}/v1/chat/completions",
            axum::routing::post(
                move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                    let origin = wire_origin.clone();
                    let tx = wire_tx.clone();
                    async move {
                        tx.send((
                            format!(
                                "{origin}{}",
                                uri.path().trim_end_matches("/chat/completions")
                            ),
                            headers["authorization"]
                                .to_str()
                                .unwrap()
                                .strip_prefix("Bearer ")
                                .unwrap()
                                .to_owned(),
                        ))
                        .unwrap();
                        StatusCode::OK
                    }
                },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.db")).await.unwrap();
        let c = store.snapshot().await.unwrap().companions[0].id.clone();
        let shutdown = tokio::sync::watch::channel(false).0;
        let credentials = std::sync::Arc::new(StoredCredentials::default());
        let app = App::new(
            crate::application::HostApplication::new(
                std::sync::Arc::new(store.clone()),
                std::sync::Arc::new(RoutedRegistry),
                credentials.clone(),
                shutdown,
            ),
            "private-token".into(),
        );
        let (status, saved) = mutate(
            &app,
            "PATCH",
            "Settings",
            json!({"revision":1,"connections":[
        {"kind":"llm","adapter":"service-a","endpoint":old_endpoint,"options":{}}]}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let p = saved["connections"][0]["id"].as_str().unwrap();
        assert_eq!(
            mutate(
                &app,
                "PUT",
                &format!("Credentials/{p}"),
                json!({"secret":"old-service-key"}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(
            crate::ports::Repository::configuration(&store)
                .await
                .unwrap()
                .credential_refs
                .contains_key(p)
        );
        assert_eq!(credentials.0.lock().unwrap().len(), 1);
        assert_eq!(mutate(&app,"PATCH",&format!("Companion/{c}"),json!({"revision":1,"model_assignments":{"companion":{"connection_id":p,"model_id":"m"}}}),None).await.0,StatusCode::OK);
        let (status, saved) = mutate(
            &app,
            "PATCH",
            "Settings",
            json!({"revision":2,"connections":[
        {"id":p,"kind":"llm","adapter":adapter,"endpoint":endpoint,"options":{}}]}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["connections"][0]["credential_state"], "missing");
        assert!(
            crate::ports::Repository::configuration(&store)
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
                "Grants",
                json!({"companion_id":c,"connection_id":p,"kind":"data_transfer",
        "consent":{"settings_revision":3,"provider_revision":"routed-v1"},"destinations":[destination],"data_kinds":["conversation"],"purpose":"conversation"}),
                Some("new-grant")
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            mutate(
                &app,
                "POST",
                &format!("SendMessage/{c}"),
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
                &format!("Credentials/{p}"),
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
                &format!("SendMessage/{c}"),
                json!({"text":"hello"}),
                Some("new-message")
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            (endpoint, "new-service-key".into())
        );
        app.application.stop().await.unwrap();
        server.abort();
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
                .method("POST")
                .uri("/rpc/ene.host.v1.HostService/RevokeGrant")
                .header(header::AUTHORIZATION, "Bearer private-token")
                .header(header::CONTENT_TYPE, "application/proto")
                .header("connect-protocol-version", "1")
                .body(Body::from(connect_fixture::encode_request(
                    "RevokeGrant",
                    json!({"id":f.grant}),
                )))
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
    let persisted = crate::ports::Repository::configuration(&f.store)
        .await
        .unwrap();
    assert_eq!(
        persisted.grants[0].status,
        crate::domain::GrantStatus::Invalidated
    );
    let effective = f.app.application.grants(None).await.unwrap();
    assert_eq!(effective[0].status, crate::domain::GrantStatus::Invalidated);
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("SendMessage/{}", f.companion),
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
            if matches!(
                n.event,
                Some(ene_rpc::notification::Event::GrantsUpdated(_))
            ) {
                break n;
            }
        }
    })
    .await
    .unwrap();
    let Some(ene_rpc::notification::Event::GrantsUpdated(grants)) = notification.event else {
        panic!("grants event missing")
    };
    assert_eq!(grants.items[0].status, "invalidated");
    f.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn long_conversations_keep_recent_turns_within_broker_budget() {
    let mut f = ConversationFixture::new().await;
    for i in 0..17 {
        let text = format!("{i}:{}", "a".repeat(65_000));
        let (status, accepted) = mutate(
            &f.app,
            "POST",
            &format!("SendMessage/{}", f.companion),
            json!({"text":text}),
            Some(&format!("long-{i}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let generation = f.started().await;
        if i == 16 {
            let request = ene_plugin::GenerationRequest {
                endpoint: generation.endpoint().to_owned(),
                options: generation.options().clone(),
                model: generation.model().to_owned(),
                preamble: generation.preamble().to_owned(),
                messages: generation
                    .messages()
                    .iter()
                    .map(|m| ene_plugin::provider::ChatMessage {
                        role: m.role.as_str().into(),
                        text: m.text.clone(),
                    })
                    .collect(),
                max_tokens: generation.max_tokens(),
                tools: Vec::new(),
            };
            assert!(serde_json::to_vec(&request).unwrap().len() < 512 * 1024);
            assert_eq!(request.messages.last().unwrap().text, text);
            assert!(request.messages.len() < 33);
            assert!(!request.messages.iter().any(|m| m.text.starts_with("0:")));
        }
        f.finish.add_permits(1);
        assert_eq!(
            f.terminal(accepted["response_message_id"].as_str().unwrap())
                .await,
            "completed"
        );
    }
}

impl crate::ports::Credentials for TestCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        Err(crate::domain::Error::unavailable())
    }
    fn session_only(&self) -> bool {
        true
    }
    fn load(&self, _: String) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        Box::pin(async { Err(crate::domain::Error::unavailable()) })
    }
    fn save(
        &self,
        _: String,
        _: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn forget(&self, _: String) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn revoking_credentials_cancels_a_running_connection_check_without_waiting() {
    let mut f = ConversationFixture::new().await;
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            "Grants",
            json!({
                "companion_id": f.companion,
                "connection_id": f.connection,
                "kind": "data_transfer",
                "consent": {"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},
                "destinations": ["https://api.openai.com", "https://models.dev"],
                "data_kinds": ["test_input", "metadata"],
                "purpose": "connection_check"
            }),
            Some("check-grant")
        )
        .await
        .0,
        StatusCode::OK
    );
    let app = f.app.clone();
    let path = format!("CheckConnection/{}", f.connection);
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
            &format!("Credentials/{}", f.connection),
            Value::Null,
            None,
        ),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
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
        for sequence in 1..=75 {
            let message = ene_rpc::Message {
                id: format!("escaped-{sequence}"),
                companion_id: target.clone(),
                role: "companion".into(),
                text: "\0".repeat(1000),
                status: "completed".into(),
                created_at: "2026-10-01T00:00:00Z".into(),
                sequence,
                resources: vec![],
                in_reply_to: None,
                task_id: None,
                question_id: None,
                failure: Default::default(),
                ..Default::default()
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
    let client = crate::api::test_client::ApiClient::new(
        &format!("http://{}/rpc", listener.local_addr().unwrap()),
        "private-token",
    )
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    let snapshot = client
        .host
        .get_snapshot_with_options(
            ene_rpc::SnapshotRequest {
                subscription_id: None,
                ..Default::default()
            },
            crate::api::test_client::call_options(5),
        )
        .await
        .unwrap()
        .into_owned();
    assert!(snapshot.messages.len() == 50);
    let mut seen: std::collections::BTreeSet<_> =
        snapshot.messages.iter().map(|m| m.sequence).collect();
    let mut cursor = snapshot.message_cursors[&companion].value.clone();
    while let Some(before) = cursor {
        let history = client
            .host
            .get_history_with_options(
                ene_rpc::HistoryRequest {
                    companion_id: (&companion).into(),
                    cursor: Some((&before).into()),
                    limit: Some(100),
                    ..Default::default()
                },
                crate::api::test_client::call_options(5),
            )
            .await
            .unwrap()
            .into_owned();
        assert!(!history.items.is_empty());
        for message in history.items {
            assert_eq!(message.text, "\0".repeat(1000));
            assert!(seen.insert(message.sequence));
        }
        cursor = history.next_cursor;
    }
    assert_eq!(seen, (1..=75).collect());
    assert!(
        client
            .host
            .get_snapshot_with_options(
                ene_rpc::SnapshotRequest {
                    subscription_id: None,
                    ..Default::default()
                },
                crate::api::test_client::call_options(5)
            )
            .await
            .map(|response| response.into_owned())
            .is_ok(),
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
            &format!("Grant/{}", f.grant),
            Value::Null,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.terminal(&first).await, "cancelled");
    assert_eq!(f.terminal(&unsent).await, "cancelled");
    assert!(
        f.started.try_recv().is_err(),
        "queued text was never sent to a provider"
    );
    assert_eq!(mutate(&f.app,"POST","Grants",json!({"companion_id":f.companion,"connection_id":f.connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),Some("restored-grant")).await.0,StatusCode::OK);
    let third = f.send("new-input").await;
    let generation = f.started().await;
    let leaked = generation
        .messages()
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
    let revoke_path = format!("Grant/{}", f.grant);
    let grant = json!({"companion_id":f.companion,"connection_id":f.connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"});
    let revoked = mutate(&f.app, "DELETE", &revoke_path, Value::Null, None).await;
    let restored = mutate(&f.app, "POST", "Grants", grant, Some("replacement-grant")).await;
    assert_eq!(revoked.0, StatusCode::OK);
    assert_eq!(restored.0, StatusCode::OK);
    assert_eq!(f.terminal(&first).await, "cancelled");
    let restarted =
        tokio::time::timeout(std::time::Duration::from_millis(200), f.started.recv()).await;
    let was_sent = restarted.is_ok();
    f.finish.add_permits(1);
    assert_eq!(f.terminal(&second).await, "cancelled");
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
            &format!("Credentials/{}", f.connection),
            Value::Null,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.terminal(&first).await, "cancelled");
    assert_eq!(f.terminal(&second).await, "cancelled");
    assert!(f.started.try_recv().is_err());
    f.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn dropped_core_waiter_still_commits_publishes_and_is_joined_by_shutdown() {
    use crate::ports::Repository;
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("owned.db")).await.unwrap();
    let application = crate::application::HostApplication::new(
        std::sync::Arc::new(store.clone()),
        std::sync::Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        std::sync::Arc::new(TestCredentials),
        tokio::sync::watch::channel(false).0,
    );
    application.companions().await.unwrap();
    let mut events = application.subscribe().await;
    let (entered, observing) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let database = store.clone();
    let blocker = tokio::spawn(async move {
        database
            .run(move |_| {
                entered.send(()).unwrap();
                let _ = blocked.recv();
                Ok(())
            })
            .await
            .unwrap();
    });
    observing.await.unwrap();
    let owned = application.clone();
    let caller = tokio::spawn(async move {
        owned
            .patch_settings(crate::application::SettingsPatch {
                revision: 1,
                connections: ene_rpc::ConnectionInputs::default().into(),
                ..Default::default()
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if tokio::time::timeout(
                std::time::Duration::from_millis(5),
                application.companions(),
            )
            .await
            .is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mutation owns controls before abandoning its waiter");
    caller.abort();
    let _ = caller.await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            application.companions()
        )
        .await
        .is_err(),
        "caller cancellation cannot abandon commit ownership"
    );
    release.send(()).unwrap();
    blocker.await.unwrap();
    application.stop().await.unwrap();
    assert_eq!(store.configuration().await.unwrap().settings.revision, 2);
    let published = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.receiver.recv().await.unwrap();
            if matches!(
                event.event,
                Some(ene_rpc::notification::Event::SettingsUpdated(_))
            ) {
                break event;
            }
        }
    })
    .await
    .unwrap();
    let Some(ene_rpc::notification::Event::SettingsUpdated(settings)) = published.event else {
        panic!("settings event missing")
    };
    assert_eq!(settings.revision, 2);
}

#[tokio::test]
async fn abandoned_connection_check_finishes_the_original_receipt_after_revocation() {
    let mut fixture = ConversationFixture::new().await;
    let companion = fixture.companion.clone();
    let connection = fixture.connection.clone();
    let grant = json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["test_input"],"purpose":"connection_check"});
    assert_eq!(
        mutate(
            &fixture.app,
            "POST",
            "Grants",
            grant,
            Some("owned-check-grant")
        )
        .await
        .0,
        StatusCode::OK
    );
    let application = fixture.app.application.clone();
    let check_connection = connection.clone();
    let check_companion = companion.clone();
    let caller = tokio::spawn(async move {
        application
            .check(
                check_connection,
                "owned-check".into(),
                crate::application::CheckInput {
                    companion_id: check_companion,
                    purpose: "companion".into(),
                    model_id: Some("m".into()),
                    ..Default::default()
                },
            )
            .await
    });
    fixture.started().await;
    caller.abort();
    let _ = caller.await;
    fixture
        .app
        .application
        .revoke_credentials(connection.clone())
        .await
        .unwrap();
    let confirmed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = fixture
                .app
                .application
                .check(
                    connection.clone(),
                    "owned-check".into(),
                    crate::application::CheckInput {
                        companion_id: companion.clone(),
                        purpose: "companion".into(),
                        model_id: Some("m".into()),
                        ..Default::default()
                    },
                )
                .await;
            match result {
                Err(error) if error.kind == crate::domain::ErrorKind::RequestInProgress => {
                    tokio::task::yield_now().await
                }
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed.0, crate::ports::Outcome::Unavailable);
    assert!(
        confirmed.1["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(fixture.started.try_recv().is_err());
    fixture.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn provider_panics_finalize_messages_and_do_not_leave_followers_waiting() {
    let mut fixture = ConversationFixture::new().await;
    fixture.started.close(); // The hostile fixture panics when reporting startup.
    let first = fixture.send("panic-first").await;
    let second = fixture.send("panic-follower").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for id in [first, second] {
            loop {
                let message = fixture
                    .app
                    .application
                    .message(fixture.companion.clone(), id.clone())
                    .await
                    .unwrap();
                if message.status == "failed" {
                    assert!(message.failure.is_set());
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    })
    .await
    .unwrap();
    fixture.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn execution_quota_rejects_before_creating_rows_and_replays_existing_acceptance() {
    let mut fixture = ConversationFixture::new().await;
    for index in 0..crate::domain::MAX_EXECUTIONS {
        fixture.send(&format!("quota-{index}")).await;
    }
    fixture.started().await;
    let path = format!("SendMessage/{}", fixture.companion);
    let body = json!({"text":"quota-0"});
    assert_eq!(
        mutate(&fixture.app, "POST", &path, body, Some("quota-0"))
            .await
            .0,
        StatusCode::OK
    );
    let (status, error) = mutate(
        &fixture.app,
        "POST",
        &path,
        json!({"text":"overflow"}),
        Some("overflow"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["details"]["reason"], "busy");
    assert_eq!(
        fixture
            .store
            .messages(fixture.companion.clone(), None, 50)
            .await
            .unwrap()
            .items
            .len(),
        crate::domain::MAX_EXECUTIONS * 2
    );
    fixture.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn review_configuration_storage_failure_notifies_without_publishing_unsaved_settings() {
    let f = ConversationFixture::new().await;
    let before = f.app.application.snapshot(None).await.unwrap();
    assert!(before.health.ready);
    let mut subscription = f.app.application.subscribe().await;
    f.store.run(|db| { db.execute_batch("CREATE TRIGGER fail_config BEFORE INSERT ON companions BEGIN SELECT RAISE(FAIL,'disk full'); END;")?; Ok(()) }).await.unwrap();
    let (status, _) = mutate(
        &f.app,
        "PATCH",
        &format!("Companion/{}", f.companion),
        json!({"revision":before.stored.companions[0].revision,"name":"unsaved"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        subscription.receiver.recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        notification.event,
        Some(ene_rpc::notification::Event::HostStorageFailed(_))
    ));
    assert!(subscription.receiver.try_recv().is_err());
    let after = f.app.application.snapshot(None).await.unwrap();
    assert!(!after.health.ready);
    assert!(after.health.recovery_required);
    assert_eq!(after.cursor.seq, notification.cursor.seq);
    assert_eq!(
        after.stored.companions[0].name,
        before.stored.companions[0].name
    );
    assert_eq!(
        after.stored.companions[0].revision,
        before.stored.companions[0].revision
    );
    assert_eq!(
        serde_json::from_str::<Value>(&after.stored.companions[0].availability.json).unwrap()["chat"]
            ["ready"],
        false
    );
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("SendMessage/{}", f.companion),
            json!({"text":"blocked"}),
            Some("after-failure")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        f.app.application.stop().await.unwrap_err().kind,
        crate::domain::ErrorKind::StorageFailure
    );
}

#[tokio::test]
async fn connect_disconnect_keeps_accepted_generation_and_explicit_stop_cancels_queue() {
    use crate::api::test_client::ApiClient;
    use ene_rpc as w;
    let mut f = ConversationFixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ApiClient::new(
        &format!("http://{}/rpc", listener.local_addr().unwrap()),
        "private-token",
    )
    .unwrap();
    let routes = router(f.app.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, routes).await.unwrap();
    });
    let mut subscription = client.host.subscribe(w::Empty::default()).await.unwrap();
    crate::api::test_client::initial(&mut subscription).await;
    let companion = f.companion.clone();
    let send = |key: &'static str| {
        let client = client.clone();
        let companion = companion.clone();
        async move {
            client
                .host
                .send_message(w::SendMessageRequest {
                    companion_id: companion,
                    request_key: key.into(),
                    text: key.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_owned()
        }
    };
    let accepted = send("connect-detached").await;
    f.started().await;
    drop(subscription);
    f.finish.add_permits(1);
    assert_eq!(f.terminal(&accepted.response_message_id).await, "completed");
    // The same generated request reuses the receipt, even after completion.
    assert_eq!(send("connect-detached").await, accepted);
    assert!(f.started.try_recv().is_err());
    let active = send("connect-stop-active").await;
    f.started().await;
    let queued = send("connect-stop-queued").await;
    client
        .host
        .stop_with_options(
            ene_rpc::StopRequest {
                companion_id: (&f.companion).into(),
                ..Default::default()
            },
            crate::api::test_client::call_options(5),
        )
        .await
        .map(|_| ())
        .unwrap();
    for id in [active.response_message_id, queued.response_message_id] {
        assert_eq!(f.terminal(&id).await, "cancelled");
    }
    assert!(f.started.try_recv().is_err());
    server.abort();
    f.app.application.stop().await.unwrap();
}

#[tokio::test]
async fn task_inference_uses_the_task_assignment_and_grant() {
    use crate::ports::inference::InferenceRequest;
    use tokio_util::sync::CancellationToken;

    let mut f = ConversationFixture::new().await;
    let inference = f.app.application.inference();
    let request = |companion: &str| InferenceRequest {
        companion_id: companion.into(),
        preamble: "system".into(),
        messages: vec![(true, "hello".into())],
        max_tokens: 128,
    };
    let missing = inference
        .complete(request(&f.companion), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(missing.reason, Some("model_unset"));

    assert_eq!(
        mutate(
            &f.app,
            "PATCH",
            &format!("Companion/{}", f.companion),
            json!({
                "revision": 2,
                "model_assignments": {
                    "task": {"connection_id": f.connection, "model_id": "task-model"}
                }
            }),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            "Grants",
            json!({
                "companion_id": f.companion,
                "connection_id": f.connection,
                "kind": "data_transfer",
                "consent": {
                    "settings_revision": 2,
                    "provider_revision": "builtin-openai-v2-model-catalog"
                },
                "destinations": ["https://api.openai.com", "https://models.dev"],
                "data_kinds": ["task"],
                "purpose": "task"
            }),
            Some("task-grant")
        )
        .await
        .0,
        StatusCode::OK
    );

    let task = {
        let inference = inference.clone();
        let companion = f.companion.clone();
        tokio::spawn(async move {
            inference
                .complete(request(&companion), CancellationToken::new())
                .await
        })
    };
    let generation = f.started().await;
    assert_eq!(generation.model(), "task-model");
    assert_eq!(generation.preamble(), "system");
    f.finish.add_permits(1);
    assert_eq!(task.await.unwrap().unwrap(), "途中");
}
