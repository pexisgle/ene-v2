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
    async fn started(&mut self) -> crate::llm::Generation {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.started.recv())
            .await
            .unwrap()
            .unwrap()
    }
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
                // A connection check needs at least one text chunk.
                finish: Some(std::sync::Arc::new(tokio::sync::Semaphore::new(1))),
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
        "consent":{"settings_revision":3,"provider_revision":"routed-v1"},"destinations":[destination],"data_kinds":["test_input"],"purpose":"connection_check"}),
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
                &format!("CheckConnection/{p}"),
                json!({"companion_id":c,"purpose":"companion","model_id":"m"}),
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
                &format!("CheckConnection/{p}"),
                json!({"companion_id":c,"purpose":"companion","model_id":"m"}),
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

/// The host API path over the real kernel: acceptance, a tool round, turn
/// notifications, history, a stop that does not wait for the running call,
/// and the conversation surviving a restart. The provider is scripted; no
/// network or credential is used.
#[tokio::test]
async fn host_api_drives_the_rig_kernel_and_reports_its_turns() {
    use crate::rig;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use std::sync::Arc;

    #[derive(serde::Deserialize)]
    struct NoArgs {}

    struct Hold {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        gate: std::sync::Arc<tokio::sync::Semaphore>,
    }
    impl rig_ecs::prelude::Tool for Hold {
        const NAME: &'static str = "hold";
        type Args = NoArgs;
        type Output = String;
        type Error = std::convert::Infallible;

        fn description(&self) -> String {
            "Waits until released.".into()
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(
            &self,
            _: &mut rig_ecs::prelude::ToolContext,
            _: NoArgs,
        ) -> Result<String, Self::Error> {
            let _ = self.entered.send(());
            // Never released: only a stop ends a turn waiting here.
            let _ = self.gate.acquire().await;
            Ok("released".into())
        }
    }

    fn connection(model: MockCompletionModel) -> rig_ecs::model::Connection {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("builtin catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(spec.reference(), model));
        rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        }
    }

    async fn turn_state(
        subscription: &mut crate::application::Subscription,
        state: &str,
        within: std::time::Duration,
    ) -> ene_rpc::TurnEvent {
        tokio::time::timeout(within, async {
            loop {
                let notification = subscription.receiver.recv().await.unwrap();
                if let Some(ene_rpc::notification::Event::TurnEvent(event)) = notification.event
                    && event.state == state
                {
                    return *event;
                }
            }
        })
        .await
        .expect("turn event")
    }

    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();
    let store = Store::open(state_dir.join("host.sqlite3")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let app = crate::bootstrap::application(store.clone(), "private-token".into());
    let rig_dir = state_dir.join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (kernel, events) = rig::Runtime::start_test(rig_dir.clone(), move |app| {
        rig_ecs::tools::AppToolsExt::add_tool(
            app,
            Hold {
                entered: entered_tx,
                gate: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
            },
        );
    })
    .unwrap();
    app.application.attach_rig(kernel, events);
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("call-1", "current_time", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("いまは正午です。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::tool_call("call-2", "hold", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        // The second held turn is stopped by the revocation; the last two
        // turns are only needed if a stop ever failed.
        vec![
            MockStreamEvent::tool_call("call-3", "hold", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("終わりました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("終わりました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    app.application
        .set_test_model(Arc::new(move |_, _| Ok(connection(model.clone()))));

    let (status, saved) = mutate(
        &app,
        "PATCH",
        "Settings",
        json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let connection = saved["connections"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        mutate(
            &app,
            "PATCH",
            &format!("Companion/{companion}"),
            json!({"revision":1,"mind":"日本語で簡潔に答える。","model_assignments":{"companion":{"connection_id":connection,"model_id":"gpt-6-luna"}}}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        mutate(
            &app,
            "PUT",
            &format!("Credentials/{connection}"),
            json!({"secret":"test-only"}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, grant) = mutate(
        &app,
        "POST",
        "Grants",
        json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
        Some("grant"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let grant = grant["grant"]["id"].as_str().unwrap().to_owned();

    let mut subscription = app.application.subscribe().await;
    let (status, accepted) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"いま何時？"}),
        Some("first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request_id = accepted["request_id"].as_str().unwrap().to_owned();
    assert!(!request_id.is_empty());
    let answered = turn_state(
        &mut subscription,
        "answered",
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(answered.request_id.as_deref(), Some(request_id.as_str()));
    assert!(!answered.stream_id.is_empty());
    let history = app
        .application
        .messages(companion.clone(), None, 50)
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|message| (message.role.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        [("user", "いま何時？"), ("assistant", "いまは正午です。")]
    );
    assert_eq!(
        history.items[0].request_id.as_deref(),
        Some(request_id.as_str())
    );
    assert_eq!(
        history.items[1].request_id.as_deref(),
        Some(request_id.as_str()),
        "the reply names the accepted input"
    );

    // The same accepted key replays the same acceptance without a new turn.
    let (status, replayed) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"いま何時？"}),
        Some("first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed, accepted);

    // A crash between the kernel's log write and the receipt completion
    // leaves a pending receipt. The next send resolves it from the saved
    // conversation and returns the same acceptance without delivering again.
    app.application
        .reserve_test_chat_receipt(&companion, "crash-key", "いま何時？", &request_id)
        .await
        .unwrap();
    let (status, recovered) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"いま何時？"}),
        Some("crash-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recovered, accepted);

    // A pending receipt the kernel never accepted is finalized unavailable;
    // a retry gets the same explicit result and starts no turn.
    app.application
        .reserve_test_chat_receipt(&companion, "lost-key", "失われた入力", "missing-request")
        .await
        .unwrap();
    for _ in 0..2 {
        let (status, _) = mutate(
            &app,
            "POST",
            &format!("SendMessage/{companion}"),
            json!({"text":"失われた入力"}),
            Some("lost-key"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
    let history = app
        .application
        .messages(companion.clone(), None, 50)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 2, "recovery delivered a duplicate");

    // A turn waiting in the held tool stops without waiting for it.
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"止めて"}),
        Some("second"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    entered_rx.recv().await.expect("the held tool started");
    let started = std::time::Instant::now();
    app.application
        .cancel_companion(companion.clone())
        .await
        .unwrap();
    let stopped = turn_state(
        &mut subscription,
        "stopped",
        std::time::Duration::from_secs(2),
    )
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(
        stopped
            .failure
            .as_option()
            .map(|failure| failure.code.as_str()),
        Some("cancelled")
    );

    // A committed revocation stops a turn that is waiting in a tool, and the
    // next input is refused rather than sent under the old grant.
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"権限を取り消して"}),
        Some("third"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    entered_rx
        .recv()
        .await
        .expect("the held tool started again");
    assert_eq!(
        mutate(&app, "DELETE", &format!("Grant/{grant}"), json!({}), None)
            .await
            .0,
        StatusCode::OK
    );
    turn_state(
        &mut subscription,
        "stopped",
        std::time::Duration::from_secs(2),
    )
    .await;
    assert_eq!(
        mutate(
            &app,
            "POST",
            &format!("SendMessage/{companion}"),
            json!({"text":"まだ送れる？"}),
            Some("fourth"),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    app.application.stop().await.unwrap();
    drop(app);

    // The next host on the same state reads the saved conversation.
    let store = Store::open(state_dir.join("host.sqlite3")).await.unwrap();
    let restarted = crate::bootstrap::application(store, "private-token".into());
    let (kernel, events) = rig::Runtime::start(rig_dir).unwrap();
    restarted.application.attach_rig(kernel, events);
    let history = restarted
        .application
        .messages(companion, None, 50)
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|message| (message.role.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("user", "いま何時？"),
            ("assistant", "いまは正午です。"),
            ("user", "止めて"),
            ("user", "権限を取り消して"),
        ]
    );
    restarted.application.stop().await.unwrap();
}

/// A provider adapter that is not the built-in OpenAI connection cannot be
/// used for the parent conversation; this is refused rather than sent through
/// an OpenAI-shaped connection the adapter never advertised.
#[tokio::test]
async fn a_plugin_adapter_is_refused_for_the_parent_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let shutdown = tokio::sync::watch::channel(false).0;
    let credentials = std::sync::Arc::new(StoredCredentials::default());
    let app = App::new(
        crate::application::HostApplication::new(
            std::sync::Arc::new(store),
            std::sync::Arc::new(RoutedRegistry),
            credentials,
            shutdown,
        ),
        "private-token".into(),
    );
    let (status, saved) = mutate(
        &app,
        "PATCH",
        "Settings",
        json!({"revision":1,"connections":[{"kind":"llm","adapter":"service-a","endpoint":"https://service.test/v1","options":{}}]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let connection = saved["connections"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        mutate(
            &app,
            "PUT",
            &format!("Credentials/{connection}"),
            json!({"secret":"test-key"}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        mutate(
            &app,
            "PATCH",
            &format!("Companion/{companion}"),
            json!({"revision":1,"model_assignments":{"companion":{"connection_id":connection,"model_id":"m"}}}),
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
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"routed-v1"},"destinations":["https://service.test"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("grant")
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, error) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"hello"}),
        Some("first"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "unsupported_capability");
    app.application.stop().await.unwrap();
}
