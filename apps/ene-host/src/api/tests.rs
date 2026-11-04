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
use std::sync::Arc;
use tower::ServiceExt;
async fn app() -> (App, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    (
        crate::bootstrap::application(store, "private-token".into()),
        temp,
    )
}
fn host_app(
    store: Store,
    registry: std::sync::Arc<dyn crate::llm::ProviderRegistry>,
    credentials: std::sync::Arc<dyn crate::ports::Credentials>,
) -> App {
    App::new(
        crate::application::HostApplication::with_plugins(
            Arc::new(store),
            registry,
            credentials,
            tokio::sync::watch::channel(false).0,
            None,
        ),
        "private-token".into(),
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
        let app = host_app(
            store.clone(),
            Arc::new(ControlledRegistry {
                registry: crate::infrastructure::llm::registry::Registry::discover(),
                started,
                finish: finish.clone(),
            }),
            Arc::new(StoredCredentials {
                session_only: true,
                ..Default::default()
            }),
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
        ("GET", "Mind") => {
            body["id"] = json!(id);
            "GetMind"
        }
        ("PATCH", "MindProfile") => {
            body["companion_id"] = json!(id);
            "UpdateMindProfile"
        }
        ("POST", "MemoryControl") => "ControlMemory",
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
    let failure = crate::api::test_client::failure(&error);
    (
        StatusCode::from_u16(failure.http_status as u16).unwrap(),
        connect_fixture::normalized(serde_json::to_value(failure).unwrap()),
    )
}

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
        let app = host_app(
            store,
            Arc::new(ReloadableRegistry {
                revision: AtomicU64::new(1),
            }),
            Arc::new(StoredCredentials {
                session_only: true,
                ..Default::default()
            }),
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

/// An in-memory credential store with the failure knobs the tests need.
#[derive(Default)]
struct StoredCredentials {
    values: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    environment: Option<&'static str>,
    session_only: bool,
    /// The first save persists the value and then reports a storage failure.
    fail_first_save: bool,
    /// `forget` never finishes, as an unresponsive Secret Service call would.
    unresponsive_forget: bool,
    /// A load waits on `release` after reporting to `entered`.
    load_gate: Option<(Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>)>,
    saves: std::sync::atomic::AtomicUsize,
    forgets: std::sync::atomic::AtomicUsize,
}
impl crate::ports::Credentials for StoredCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        self.environment
            .map(str::to_owned)
            .ok_or_else(crate::domain::Error::unavailable)
    }
    fn session_only(&self) -> bool {
        self.session_only
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        Box::pin(async move {
            let value = self
                .values
                .lock()
                .unwrap()
                .get(&reference)
                .cloned()
                .ok_or_else(crate::domain::Error::unavailable)?;
            if let Some((entered, release)) = &self.load_gate {
                entered.add_permits(1);
                release.acquire().await.unwrap().forget();
            }
            Ok(value)
        })
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            let first = self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            self.values.lock().unwrap().insert(reference, value);
            if first && self.fail_first_save {
                Err(crate::domain::Error::unavailable())
            } else {
                Ok(())
            }
        })
    }
    fn forget(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.forgets
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.unresponsive_forget {
                std::future::pending::<()>().await
            }
            self.values.lock().unwrap().remove(&reference);
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
        let credentials = std::sync::Arc::new(StoredCredentials::default());
        let app = host_app(
            store.clone(),
            std::sync::Arc::new(RoutedRegistry),
            credentials.clone(),
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
            store
                .configuration()
                .await
                .unwrap()
                .credential_refs
                .contains_key(p)
        );
        assert_eq!(credentials.values.lock().unwrap().len(), 1);
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
            store
                .configuration()
                .await
                .unwrap()
                .credential_refs
                .is_empty()
        );
        assert!(credentials.values.lock().unwrap().is_empty());
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
    let persisted = f.store.configuration().await.unwrap();
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
    let (kernel, events) = rig::Runtime::start_with(rig_dir.clone(), move |app| {
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
    let (kernel, events) = rig::Runtime::start_with(rig_dir, |_| {}).unwrap();
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
    let credentials = std::sync::Arc::new(StoredCredentials::default());
    let app = host_app(store, std::sync::Arc::new(RoutedRegistry), credentials);
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

#[tokio::test]
async fn mind_profile_is_revisioned_notified_and_persists_across_hosts() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let app = crate::bootstrap::application(store.clone(), "private-token".into());
    let (status, initial) =
        mutate(&app, "GET", &format!("Mind/{companion}"), json!(null), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(initial["state"]["revision"], 1);
    assert_eq!(initial["profile"]["revision"], 1);
    assert!(!initial["profile"]["core"].as_str().unwrap().is_empty());

    let mut subscription = app.application.subscribe().await;
    let (status, updated) = mutate(
        &app,
        "PATCH",
        &format!("MindProfile/{companion}"),
        json!({
            "expected_revision": 1,
            "facets": ["静かだがよく見ている"],
            "examples": ["「なるほど」と短く返す"],
            "background": ["架空の街で育った"],
            "expression": ["声を落として話す"]
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["profile"]["revision"], 2);
    assert_eq!(updated["profile"]["facets"][0], "静かだがよく見ている");
    assert_eq!(updated["profile"]["core"], initial["profile"]["core"]);
    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        subscription.receiver.recv(),
    )
    .await
    .unwrap()
    .unwrap();
    match notification.event {
        Some(ene_rpc::notification::Event::MindUpdated(view)) => {
            assert_eq!(view.state.expect("state").revision, 1);
            assert_eq!(view.profile.expect("profile").revision, 2);
        }
        other => panic!("expected MindUpdated, got {other:?}"),
    }

    let (status, _) = mutate(
        &app,
        "PATCH",
        &format!("MindProfile/{companion}"),
        json!({"expected_revision": 1, "facets": ["古い版"]}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    drop(app);
    let restarted = crate::bootstrap::application(store, "private-token".into());
    let (status, after) = mutate(
        &restarted,
        "GET",
        &format!("Mind/{companion}"),
        json!(null),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["profile"]["revision"], 2);
    assert_eq!(after["profile"]["examples"][0], "「なるほど」と短く返す");
}

/// A scripted parent requests one real mind update. The host commits it, feeds
/// the committed state into the same turn's next model call, and notifies the
/// client; the source is the accepted input, and the author example is not
/// stored as an experience.
#[tokio::test]
async fn a_parent_mind_update_reaches_sqlite_and_the_next_model_call() {
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let app = crate::bootstrap::application(store.clone(), "private-token".into());
    let rig_dir = temp.path().join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    crate::bootstrap::attach_kernel(&app, rig_dir).unwrap();

    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "call-mind",
                "mind_update",
                json!({
                    "expected_revision": 1,
                    "memories": [{
                        "content": "ユーザーは柚子が好き",
                        "description": "好きな果物",
                        "acquisition": "user_statement",
                        "sources": []
                    }],
                    "state": {
                        "emotion": {
                            "description": "少しうれしい",
                            "target": "ユーザー",
                            "reason": "好みを話してくれた",
                            "signals": [{"label":"声がやわらぐ","intensity":0.4}]
                        },
                        "relationships": [{
                            "subject": "ユーザー",
                            "understanding": "好みを話してくれる"
                        }]
                    }
                }),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("覚えました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let captured = model.clone();
    app.application.set_test_model(Arc::new(move |_, _| {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("builtin catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            model.clone(),
        ));
        Ok(rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        })
    }));

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
            json!({
                "revision": 1,
                "mind": "日本語で、落ち着いて答える。",
                "model_assignments": {"companion": {"connection_id": connection, "model_id": "deepseek/deepseek-flash"}}
            }),
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
    assert_eq!(
        mutate(
            &app,
            "POST",
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("mind-grant"),
        )
        .await
        .0,
        StatusCode::OK
    );
    // The author example is role-play material, not an experience.
    assert_eq!(
        mutate(
            &app,
            "PATCH",
            &format!("MindProfile/{companion}"),
            json!({
                "expected_revision": 1,
                "examples": ["「なるほど」と短く返す"],
                "facets": ["静かだがよく見ている"]
            }),
            None
        )
        .await
        .0,
        StatusCode::OK
    );

    let mut subscription = app.application.subscribe().await;
    let (status, accepted) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"柚子が好きです。覚えておいて。"}),
        Some("mind-first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request_id = accepted["request_id"].as_str().unwrap().to_owned();
    let (mut mind_updated, answered) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut updated = None;
            loop {
                let notification = subscription.receiver.recv().await.unwrap();
                match notification.event {
                    Some(ene_rpc::notification::Event::MindUpdated(view)) => updated = Some(view),
                    Some(ene_rpc::notification::Event::TurnEvent(event))
                        if event.state == "answered" =>
                    {
                        return (updated, event);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("mind turn deadline");
    let updated = mind_updated.take().expect("MindUpdated");
    assert_eq!(
        updated.state.as_option().map(|state| state.revision),
        Some(2)
    );

    let requests = captured.requests();
    assert_eq!(
        requests.len(),
        2,
        "the tool round must make two model calls"
    );
    let system = |request: &rig_core::completion::CompletionRequest| {
        request
            .chat_history
            .iter()
            .find_map(|message| match &**message {
                rig_core::completion::Message::System { content } => Some(content.clone()),
                _ => None,
            })
    };
    let first = system(&requests[0]).expect("first system prompt");
    let second = system(&requests[1]).expect("second system prompt");
    assert!(first.contains("「なるほど」と短く返す"));
    assert!(first.contains("revision: 1"));
    assert!(!first.contains("少しうれしい"));
    assert!(second.contains("少しうれしい"), "{second}");
    assert!(second.contains("revision: 2"), "{second}");
    assert!(second.contains("好みを話してくれる"), "{second}");

    store
        .run(move |db| {
            let (content, sources): (String, String) = db.query_row(
                "SELECT content,sources FROM mind_memories WHERE owner=?1",
                [companion.clone()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(content, "ユーザーは柚子が好き");
            let sources: Vec<crate::domain::mind::SourceRef> = serde_json::from_str(&sources)?;
            assert_eq!(sources[0].kind, crate::domain::mind::SourceKind::Input);
            assert_eq!(sources[0].id, request_id);
            assert!(sources[0].entry_id.is_some());
            let author_examples: i64 = db.query_row(
                "SELECT COUNT(*) FROM mind_memories WHERE content LIKE '%なるほど%'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(author_examples, 0);
            let state: String = db.query_row(
                "SELECT state FROM mind_companions WHERE companion_id=?1",
                [companion.clone()],
                |row| row.get(0),
            )?;
            assert!(state.contains("少しうれしい"));
            Ok(())
        })
        .await
        .unwrap();
    assert!(!answered.stream_id.is_empty());
}

async fn wait_for_answered(subscription: &mut crate::application::Subscription) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let notification = subscription.receiver.recv().await.unwrap();
            if let Some(ene_rpc::notification::Event::TurnEvent(event)) = notification.event
                && event.state == "answered"
            {
                return;
            }
        }
    })
    .await
    .expect("answered turn");
}

/// A normal turn without a mind_update tool call is consolidated by the finite
/// host worker. The next turn receives the new memory through automatic recall
/// (lexical fallback here, since the test has no task model to embed with).
#[tokio::test]
async fn automatic_consolidation_and_recall_use_confirmed_turns_without_a_tool() {
    use crate::bootstrap;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let app = bootstrap::application(store.clone(), "private-token".into());
    let rig_dir = temp.path().join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    bootstrap::attach_kernel(&app, rig_dir.clone()).unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("楽しかったですね。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("柚子ジャムのことですね。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    app.application.set_test_model(Arc::new(move |_, _| {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("builtin catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            model.clone(),
        ));
        Ok(rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        })
    }));
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
            json!({
                "revision": 1,
                "mind": "静かな助手",
                "model_assignments": {"companion": {"connection_id": connection, "model_id": "deepseek/deepseek-flash"}}
            }),
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
    assert_eq!(
        mutate(
            &app,
            "POST",
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("auto-memory-grant"),
        )
        .await
        .0,
        StatusCode::OK
    );
    app.application.set_consolidation_hook(Arc::new(|prompt| {
        assert!(prompt.contains("confirmed_entries"));
        assert!(prompt.contains("author_material"));
        Box::pin(async move {
            if prompt.contains("一緒に柚子ジャムを作った") {
                Ok(r#"{"memories":[{"content":"一緒に柚子ジャムを作った経験","description":"共に作った思い出","appraisal":"楽しかった","acquisition":"direct_experience"}],"reason":"shared experience"}"#.to_owned())
            } else {
                // The second turn is a paraphrase; the test does not need a
                // second memory, and an explicit no-change must advance the
                // cursor without inventing one.
                Ok(r#"{"memories":[],"reason":"no_change"}"#.to_owned())
            }
        })
    }));
    let long_prompt = app
        .application
        .mind_prompt_with_recall(&companion, "静かな助手", &"あ".repeat(300), "long-input")
        .await
        .unwrap();
    assert!(long_prompt.contains("いまの話題で思い出した記憶"));
    app.application.start_mind_worker();
    let mut subscription = app.application.subscribe().await;
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"一緒に柚子ジャムを作った。楽しかった。"}),
        Some("auto-first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let mind = app.application.get_mind(companion.clone()).await.unwrap();
        if mind
            .consolidation
            .as_option()
            .is_some_and(|status| status.jobs >= 1 && status.last_sequence > 0)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the consolidation worker did not run: {mind:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let owner = companion.clone();
    store
        .run(move |db| {
            let (created_by, sources): (String, String) = db.query_row(
                "SELECT created_by,sources FROM mind_memories WHERE owner=?1",
                [owner.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(created_by, "consolidation");
            let sources: Vec<crate::domain::mind::SourceRef> = serde_json::from_str(&sources)?;
            assert!(
                sources
                    .iter()
                    .any(|source| source.kind == crate::domain::mind::SourceKind::Input)
            );
            assert!(
                sources
                    .iter()
                    .any(|source| source.kind == crate::domain::mind::SourceKind::Reply)
            );
            Ok(())
        })
        .await
        .unwrap();
    // The second turn's model input gets the consolidated memory through
    // automatic recall, without a mind_recall tool call.
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"ジャムの思い出は？"}),
        Some("auto-second"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let log_path = rig_dir.join("logs").join(format!("{companion}.jsonl"));
    let raw = std::fs::read_to_string(&log_path).unwrap();
    let prompts = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["type"] == "state" && value["key"] == "rig_ecs::agent::SystemPrompt")
        .filter_map(|value| value["value"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("一緒に柚子ジャムを作った経験")),
        "automatic recall missing from the model input: {prompts:?}"
    );

    app.application.stop().await.unwrap();
}

#[tokio::test]
async fn memory_forget_and_share_are_owner_only_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let source = crate::domain::mind::SourceRef {
        kind: crate::domain::mind::SourceKind::Input,
        id: "request".into(),
        entry_id: Some("entry".into()),
        speaker: Some("user".into()),
        observed_at: Some("2026-01-01T00:00:00Z".into()),
    };
    let id = store
        .mind_update(
            companion.clone(),
            "op-control".into(),
            "hash-control".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: vec![crate::domain::mind::NewMemory {
                    content: "柚子ジャムを作った".into(),
                    description: String::new(),
                    appraisal: String::new(),
                    acquisition: crate::domain::mind::Acquisition::DirectExperience,
                    supersedes: Vec::new(),
                    sources: Vec::new(),
                    occurred_at: None,
                }],
                state: None,
            },
            vec![source],
            "parent".into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .unwrap()
        .memories[0]
        .id
        .clone();
    let app = crate::bootstrap::application(store.clone(), "private-token".into());
    let (status, forgotten) = mutate(
        &app,
        "POST",
        "MemoryControl",
        json!({"companion_id": companion, "memory_id": id, "action": "forget"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(forgotten["recall_preference"], "forgotten");
    let (status, again) = mutate(
        &app,
        "POST",
        "MemoryControl",
        json!({"companion_id": companion, "memory_id": id, "action": "forget"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["revision"], forgotten["revision"]);
    let (status, shared) = mutate(
        &app,
        "POST",
        "MemoryControl",
        json!({"companion_id": companion, "memory_id": id, "action": "share"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shared["audience"], "shared");
    let (status, _) = mutate(
        &app,
        "POST",
        "MemoryControl",
        json!({"companion_id": companion, "memory_id": "00000000-0000-0000-0000-000000000001", "action": "share"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn automatic_semantic_recall_holds_the_model_until_the_embedding_completes() {
    use crate::bootstrap;
    use crate::llm::Embedding;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let source = crate::domain::mind::SourceRef {
        kind: crate::domain::mind::SourceKind::Input,
        id: "seed".into(),
        entry_id: Some("seed".into()),
        speaker: Some("user".into()),
        observed_at: Some("2026-01-01T00:00:00Z".into()),
    };
    let memory_id = store
        .mind_update(
            companion.clone(),
            "seed-op".into(),
            "seed-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: vec![crate::domain::mind::NewMemory {
                    content: "一緒に柚子ジャムを作った。".into(),
                    description: "共有した経験".into(),
                    appraisal: String::new(),
                    acquisition: crate::domain::mind::Acquisition::DirectExperience,
                    supersedes: Vec::new(),
                    sources: Vec::new(),
                    occurred_at: None,
                }],
                state: None,
            },
            vec![source],
            "parent".into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .unwrap()
        .memories[0]
        .id
        .clone();
    store
        .mind_store_vectors(
            vec![crate::infrastructure::sqlite::memory::VectorEntry {
                memory_id: memory_id.clone(),
                model: crate::application::inference::EMBEDDING_MODEL.into(),
                preprocess: crate::infrastructure::sqlite::memory::TEXT_PREPROCESS.into(),
                vector: vec![1.0, 0.0],
            }],
            crate::application::inference::EMBEDDING_MODEL.into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .unwrap();

    let app = bootstrap::application(store.clone(), "private-token".into());
    let rig_dir = temp.path().join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    bootstrap::attach_kernel(&app, rig_dir).unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("思い出しました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("ローカル結果で答えました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("期限のローカル結果で答えました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let captured = model.clone();
    app.application.set_test_model(Arc::new(move |_, _| {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            model.clone(),
        ));
        Ok(rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        })
    }));
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
            json!({
                "revision": 1,
                "mind": "静かな助手",
                "model_assignments": {"companion": {"connection_id": connection, "model_id": "deepseek/deepseek-flash"}}
            }),
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
    assert_eq!(
        mutate(
            &app,
            "POST",
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("semantic-grant"),
        )
        .await
        .0,
        StatusCode::OK
    );

    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let index_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let index_calls = Arc::new(AtomicUsize::new(0));
    let hook_gate = gate.clone();
    let hook_calls = calls.clone();
    let hook_fail = fail.clone();
    let hook_index_mode = index_mode.clone();
    let hook_index_calls = index_calls.clone();
    app.application
        .set_embedding_hook(Arc::new(move |_companion, input| {
            let gate = hook_gate.clone();
            let calls = hook_calls.clone();
            let fail = hook_fail.clone();
            let index_mode = hook_index_mode.clone();
            let index_calls = hook_index_calls.clone();
            Box::pin(async move {
                let is_index = input.iter().any(|text| text.contains("索引待ちの記憶"));
                if is_index {
                    index_calls.fetch_add(1, Ordering::SeqCst);
                    if fail.load(Ordering::SeqCst) {
                        return Err(crate::domain::Error::new(
                            crate::domain::ErrorKind::ProviderFailure,
                        ));
                    }
                    return Ok(Embedding {
                        vectors: vec![vec![1.0, 0.0]],
                        input_tokens: 1,
                    });
                }
                if index_mode.load(Ordering::SeqCst) {
                    return Ok(Embedding {
                        vectors: vec![vec![1.0, 0.0]],
                        input_tokens: 1,
                    });
                }
                calls.fetch_add(1, Ordering::SeqCst);
                if fail.load(Ordering::SeqCst) {
                    return Err(crate::domain::Error::new(
                        crate::domain::ErrorKind::ProviderFailure,
                    ));
                }
                gate.notified().await;
                Ok(Embedding {
                    vectors: vec![vec![1.0, 0.0]],
                    input_tokens: 1,
                })
            })
        }));

    let mut subscription = app.application.subscribe().await;
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"あの黄色い果実の甘い保存食は何だった？"}),
        Some("semantic-wait"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while calls.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the semantic search did not start"
    );
    assert_eq!(
        captured.requests().len(),
        0,
        "the model was called while the semantic search was pending"
    );
    // A mind/profile update while the search is pending must not be rolled
    // back by an older full prompt or by the completion.
    let (status, _) = mutate(
        &app,
        "PATCH",
        &format!("MindProfile/{companion}"),
        json!({
            "expected_revision": 1,
            "facets": ["検索中に更新された側面"],
            "examples": []
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    gate.notify_one();
    wait_for_answered(&mut subscription).await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 1);
    let system = requests[0]
        .chat_history
        .iter()
        .find_map(|message| match &**message {
            rig_core::completion::Message::System { content } => Some(content.clone()),
            _ => None,
        })
        .expect("system prompt");
    assert!(
        system.contains("柚子ジャム")
            && system.contains(&memory_id)
            && system.contains("検索中に更新された側面"),
        "the final model request lost the newer mind or recall: {system}"
    );

    // A search failure must keep the local lexical result and still send once.
    fail.store(true, Ordering::SeqCst);
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"柚子ジャムの思い出は？"}),
        Some("semantic-failure"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 2);
    let system = requests[1]
        .chat_history
        .iter()
        .find_map(|message| match &**message {
            rig_core::completion::Message::System { content } => Some(content.clone()),
            _ => None,
        })
        .expect("failure system prompt");
    assert!(
        system.contains("柚子ジャム") && system.contains("意味=error"),
        "local results were lost on search failure: {system}"
    );

    // A pending search that outlives the production budget must send once
    // with the valid local result and ignore the late completion.
    fail.store(false, Ordering::SeqCst);
    let before = calls.load(Ordering::SeqCst);
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"柚子ジャムの思い出をもう一度"}),
        Some("semantic-timeout"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let timeout_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while calls.load(Ordering::SeqCst) <= before && tokio::time::Instant::now() < timeout_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(calls.load(Ordering::SeqCst) > before);
    wait_for_answered(&mut subscription).await;
    let requests = captured.requests();
    assert_eq!(requests.len(), 3);
    let system = requests[2]
        .chat_history
        .iter()
        .find_map(|message| match &**message {
            rig_core::completion::Message::System { content } => Some(content.clone()),
            _ => None,
        })
        .expect("timeout system prompt");
    assert!(
        system.contains("柚子ジャム") && system.contains("意味=timeout"),
        "timeout did not keep the local result: {system}"
    );
    gate.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        captured.requests().len(),
        3,
        "a late result after timeout sent another model call"
    );

    // The index worker uses the same hook. A transient failure must stop
    // calling it until the relevant configuration change resets pending work.
    let _ = store
        .mind_update(
            companion.clone(),
            "seed-index-op".into(),
            "seed-index-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 2,
                memories: vec![crate::domain::mind::NewMemory {
                    content: "索引待ちの記憶".into(),
                    description: String::new(),
                    appraisal: String::new(),
                    acquisition: crate::domain::mind::Acquisition::DirectExperience,
                    supersedes: Vec::new(),
                    sources: Vec::new(),
                    occurred_at: None,
                }],
                state: None,
            },
            vec![crate::domain::mind::SourceRef {
                kind: crate::domain::mind::SourceKind::Input,
                id: "seed-index".into(),
                entry_id: Some("seed-index".into()),
                speaker: Some("user".into()),
                observed_at: Some("2026-01-01T00:00:00Z".into()),
            }],
            "parent".into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .unwrap()
        .memories[0]
        .id
        .clone();
    fail.store(true, Ordering::SeqCst);
    index_mode.store(true, Ordering::SeqCst);
    let index_before = index_calls.load(Ordering::SeqCst);
    app.application.start_mind_worker();
    let index_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while index_calls.load(Ordering::SeqCst) <= index_before
        && tokio::time::Instant::now() < index_deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        index_calls.load(Ordering::SeqCst) > index_before,
        "the index worker did not reach the embedding hook"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        index_calls.load(Ordering::SeqCst),
        index_before + 1,
        "the index worker retried a failed embedding without a configuration change"
    );
    let index_failed = store.snapshot().await.unwrap();
    let _ = index_failed;
    assert!(
        store
            .mind_index_status(companion.clone(), "2026-01-01T00:00:01Z".into())
            .await
            .unwrap()
            .pending_vectors
            > 0,
        "the failed index must remain pending"
    );

    // A related grant/configuration commit resets the bounded wait and wakes
    // the worker through the normal path.
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        mutate(
            &app,
            "POST",
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("index-retry-grant"),
        )
        .await
        .0,
        StatusCode::OK
    );
    let retry_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = store
            .mind_index_status(companion.clone(), "2026-01-01T00:00:02Z".into())
            .await
            .unwrap();
        if status.pending_vectors == 0 && status.embedding_model.is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < retry_deadline,
            "the index did not resume after configuration change: {status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(index_calls.load(Ordering::SeqCst) > index_before + 1);

    app.application.stop().await.unwrap();
}

#[tokio::test]
async fn consolidation_connection_walks_a_page_boundary_into_sqlite_cursor() {
    use crate::bootstrap;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::MockCompletionModel;
    use rig_ecs::effects::Handler;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let app = bootstrap::application(store.clone(), "private-token".into());
    let rig_dir = temp.path().join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    bootstrap::attach_kernel(&app, rig_dir).unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            rig_core::test_utils::MockStreamEvent::text("ページ境界の応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::text("長文の応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::text("短文の応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::ToolCall {
                id: "invalid-tool-call".into(),
                name: String::new(),
                arguments: serde_json::json!({}),
                call_id: None,
            },
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::text("失敗後の応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::text("Steer応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            rig_core::test_utils::MockStreamEvent::text("Steer二つ目の応答"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    app.application.set_test_model(Arc::new(move |_, _| {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(
            spec.reference(),
            model.clone(),
        ));
        Ok(rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        })
    }));
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
            json!({"revision":1,"mind":"静かな助手","model_assignments":{"companion":{"connection_id":connection,"model_id":"deepseek/deepseek-flash"}}}),
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
    assert_eq!(
        mutate(
            &app,
            "POST",
            "Grants",
            json!({"companion_id":companion,"connection_id":connection,"kind":"data_transfer","consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},"destinations":["https://api.openai.com","https://models.dev"],"data_kinds":["conversation"],"purpose":"conversation"}),
            Some("page-grant"),
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut subscription = app.application.subscribe().await;
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"ページ境界の発言"}),
        Some("page-boundary"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;

    // The test page is four messages. The last user is on page one and its
    // saved answer is on page two; the real pending scan must cross it.
    let page = app
        .application
        .pending_messages(&companion, 0, 0)
        .await
        .expect("pending history");
    let ids = page
        .messages
        .iter()
        .map(|message| {
            (
                message.sequence,
                message.role.as_str(),
                message.text.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        ids.iter()
            .any(|(_, role, text)| *role == "user" && *text == "ページ境界の発言"),
        "page-boundary user missing: {ids:?}"
    );
    assert!(
        ids.iter()
            .any(|(_, role, text)| *role == "assistant" && *text == "ページ境界の応答"),
        "next-page reply missing: {ids:?}"
    );
    assert!(ids.len() >= 2, "the accepted user/answer was lost: {ids:?}");

    let (batch, sources, through, offset, chars) =
        crate::application::memory::batch_chunks(page.messages, 0, 0);
    assert!(
        batch
            .iter()
            .any(|chunk| chunk.text.contains("ページ境界の発言"))
    );
    assert!(
        batch
            .iter()
            .any(|chunk| chunk.text.contains("ページ境界の応答"))
    );
    let result = store
        .mind_consolidate(
            companion.clone(),
            "page-boundary-op".into(),
            "page-boundary-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: Vec::new(),
                state: None,
            },
            sources,
            through,
            offset,
            "consolidation-v1".into(),
            0,
            chars,
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .expect("apply cursor");
    assert!(result.advanced);
    let after = app
        .application
        .pending_messages(
            &companion,
            result.status.last_sequence,
            result.status.last_offset,
        )
        .await
        .expect("next pending scan");
    assert!(
        after.messages.is_empty(),
        "the next scan repeated a processed range: {:?}",
        after
            .messages
            .iter()
            .map(|message| (message.sequence, message.id.as_str()))
            .collect::<Vec<_>>()
    );

    let long = format!("{}{}", "長".repeat(9000), "長文末尾");
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":long}),
        Some("page-long"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"長文の後の短文"}),
        Some("page-short"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let mut cursor = (result.status.last_sequence, result.status.last_offset);
    let mut saw_long_tail = false;
    let mut saw_short = false;
    for _ in 0..20 {
        let tail_page = app
            .application
            .pending_messages(&companion, cursor.0, cursor.1)
            .await
            .expect("tail history");
        if tail_page.messages.is_empty() {
            break;
        }
        let (tail_batch, tail_sources, tail_through, tail_offset, tail_chars) =
            crate::application::memory::batch_chunks(tail_page.messages, cursor.0, cursor.1);
        for chunk in &tail_batch {
            saw_long_tail |= chunk.text.contains("長文末尾");
            saw_short |= chunk.text.contains("長文の後の短文");
        }
        let tail = store
            .mind_consolidate(
                companion.clone(),
                format!("page-tail-op-{}", cursor.0),
                "page-tail-hash".into(),
                crate::domain::mind::MindUpdateRequest {
                    expected_revision: 1,
                    memories: Vec::new(),
                    state: None,
                },
                tail_sources,
                tail_through,
                tail_offset,
                "consolidation-v1".into(),
                0,
                tail_chars,
                "2026-01-01T00:00:01Z".into(),
            )
            .await
            .expect("apply tail cursor");
        assert!(tail.advanced);
        let next = (tail.status.last_sequence, tail.status.last_offset);
        assert!(next > cursor, "the tail cursor did not advance");
        cursor = next;
    }
    assert!(saw_long_tail, "the long message tail was not processed");
    assert!(
        saw_short,
        "the short message after the long tail was dropped"
    );
    let tail_after = app
        .application
        .pending_messages(&companion, cursor.0, cursor.1)
        .await
        .expect("tail next scan");
    assert!(tail_after.messages.is_empty(), "tail range repeated");

    // A failed terminal entry must not be treated as a completed assistant
    // experience, and the next confirmed input must still advance.
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"失敗する終端の発言"}),
        Some("terminal-failed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let failed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let notification = subscription.receiver.recv().await.unwrap();
            if let Some(ene_rpc::notification::Event::TurnEvent(event)) = notification.event
                && matches!(event.state.as_str(), "failed" | "stopped")
            {
                return event.state;
            }
        }
    })
    .await
    .expect("failed terminal");
    assert!(matches!(failed.as_str(), "failed" | "stopped"));
    let (status, _) = mutate(
        &app,
        "POST",
        &format!("SendMessage/{companion}"),
        json!({"text":"終端の後の確定入力"}),
        Some("terminal-after"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_answered(&mut subscription).await;
    let terminal = app
        .application
        .pending_messages(&companion, cursor.0, cursor.1)
        .await
        .expect("terminal history");
    assert!(
        terminal
            .messages
            .iter()
            .all(|message| message.text != "失敗する終端の発言"),
        "a failed terminal user was treated as a confirmed experience"
    );
    let confirmed = terminal
        .messages
        .iter()
        .filter(|message| message.text == "終端の後の確定入力")
        .count();
    assert_eq!(
        confirmed, 1,
        "the confirmed input after the terminal was lost"
    );
    let (terminal_batch, terminal_sources, terminal_through, terminal_offset, terminal_chars) =
        crate::application::memory::batch_chunks(terminal.messages, cursor.0, cursor.1);
    assert!(
        terminal_batch
            .iter()
            .any(|chunk| chunk.text.contains("終端の後の確定入力"))
    );
    let terminal_result = store
        .mind_consolidate(
            companion.clone(),
            "terminal-op".into(),
            "terminal-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: Vec::new(),
                state: None,
            },
            terminal_sources,
            terminal_through,
            terminal_offset,
            "consolidation-v1".into(),
            0,
            terminal_chars,
            "2026-01-01T00:00:02Z".into(),
        )
        .await
        .expect("apply terminal cursor");
    assert!(terminal_result.advanced);
    let terminal_after = app
        .application
        .pending_messages(
            &companion,
            terminal_result.status.last_sequence,
            terminal_result.status.last_offset,
        )
        .await
        .expect("terminal next scan");
    assert!(
        terminal_after.messages.is_empty(),
        "the terminal range repeated: {:?}",
        terminal_after
            .messages
            .iter()
            .map(|message| (message.sequence, message.text.as_str()))
            .collect::<Vec<_>>()
    );

    // Two inputs accepted while the turn is live must both survive as user
    // entries, with no requirement that each has its own assistant reply.
    let steer_path = format!("SendMessage/{companion}");
    let (first, second) = tokio::join!(
        mutate(
            &app,
            "POST",
            &steer_path,
            json!({"text":"Steer入力1"}),
            Some("steer-one"),
        ),
        mutate(
            &app,
            "POST",
            &steer_path,
            json!({"text":"Steer入力2"}),
            Some("steer-two"),
        )
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let steer_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut steer_messages = Vec::new();
    while tokio::time::Instant::now() < steer_deadline {
        let pending = app
            .application
            .pending_messages(
                &companion,
                terminal_result.status.last_sequence,
                terminal_result.status.last_offset,
            )
            .await
            .expect("steer history");
        let has_one = pending
            .messages
            .iter()
            .any(|message| message.text == "Steer入力1");
        let has_two = pending
            .messages
            .iter()
            .any(|message| message.text == "Steer入力2");
        if has_one && has_two {
            steer_messages = pending.messages;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let one = steer_messages
        .iter()
        .position(|message| message.text == "Steer入力1")
        .expect("first steer input");
    let two = steer_messages
        .iter()
        .position(|message| message.text == "Steer入力2")
        .expect("second steer input");
    assert_eq!(
        two,
        one + 1,
        "the second input was not accepted into the same live turn: {:?}",
        steer_messages
            .iter()
            .map(|message| (
                message.sequence,
                message.role.as_str(),
                message.text.as_str()
            ))
            .collect::<Vec<_>>()
    );
    wait_for_answered(&mut subscription).await;
    let steer_pending = app
        .application
        .pending_messages(
            &companion,
            terminal_result.status.last_sequence,
            terminal_result.status.last_offset,
        )
        .await
        .expect("steer pending");
    let first_request = steer_pending
        .messages
        .iter()
        .find(|message| message.text == "Steer入力1")
        .and_then(|message| message.request_id.clone())
        .expect("first steer request id");
    let replies = steer_pending
        .messages
        .iter()
        .filter(|message| message.role == "assistant")
        .collect::<Vec<_>>();
    assert_eq!(
        replies.len(),
        1,
        "the steer inputs did not share one turn answer: {:?}",
        steer_pending
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.text.as_str()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        replies[0].request_id.as_deref(),
        Some(first_request.as_str())
    );
    let (steer_batch, steer_sources, steer_through, steer_offset, steer_chars) =
        crate::application::memory::batch_chunks(
            steer_pending.messages,
            terminal_result.status.last_sequence,
            terminal_result.status.last_offset,
        );
    assert!(
        steer_batch
            .iter()
            .any(|chunk| chunk.text.contains("Steer入力1"))
            && steer_batch
                .iter()
                .any(|chunk| chunk.text.contains("Steer入力2"))
    );
    let steer_result = store
        .mind_consolidate(
            companion.clone(),
            "steer-op".into(),
            "steer-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: Vec::new(),
                state: None,
            },
            steer_sources,
            steer_through,
            steer_offset,
            "consolidation-v1".into(),
            0,
            steer_chars,
            "2026-01-01T00:00:03Z".into(),
        )
        .await
        .expect("apply steer cursor");
    assert!(steer_result.advanced);
    let steer_after = app
        .application
        .pending_messages(
            &companion,
            steer_result.status.last_sequence,
            steer_result.status.last_offset,
        )
        .await
        .expect("steer next scan");
    assert!(steer_after.messages.is_empty(), "steer range repeated");

    app.application.stop().await.unwrap();
}

#[tokio::test]
async fn index_worker_retry_budget_is_finite_and_reset_resumes_it() {
    use crate::application::mind::set_test_now;
    use crate::llm::Embedding;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    set_test_now(1_800_000_000);
    let app = crate::bootstrap::application(store.clone(), "t".into());
    let rig_dir = temp.path().join("rig");
    std::fs::create_dir_all(&rig_dir).unwrap();
    crate::bootstrap::attach_kernel(&app, rig_dir).unwrap();
    let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
        rig_core::test_utils::MockStreamEvent::text("unused"),
        rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
    ]]);
    app.application.set_test_model(Arc::new(move |_, _| {
        let spec = rig_core::catalog::Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("catalog")
            .shared();
        let handler =
            rig_core::serve::ErasedHandler::new(rig_core::serve::adapters::ModelAdapter::<
                rig_core::operation::Completion,
            >::new(spec.reference(), model.clone()));
        Ok(rig_ecs::model::Connection {
            spec,
            handler: rig_ecs::effects::Handler(handler),
        })
    }));
    store
        .mind_update(
            companion.clone(),
            "retry-budget-op".into(),
            "retry-budget-hash".into(),
            crate::domain::mind::MindUpdateRequest {
                expected_revision: 1,
                memories: vec![crate::domain::mind::NewMemory {
                    content: "索引待ちの記憶".into(),
                    description: String::new(),
                    appraisal: String::new(),
                    acquisition: crate::domain::mind::Acquisition::DirectExperience,
                    supersedes: Vec::new(),
                    sources: Vec::new(),
                    occurred_at: None,
                }],
                state: None,
            },
            Vec::new(),
            "parent".into(),
            "2026-01-01T00:00:00Z".into(),
        )
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicBool::new(true));
    let hook_calls = calls.clone();
    let hook_fail = fail.clone();
    app.application
        .set_embedding_hook(Arc::new(move |_companion, _input| {
            let calls = hook_calls.clone();
            let fail = hook_fail.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if fail.load(Ordering::SeqCst) {
                    return Err(crate::domain::Error::new(
                        crate::domain::ErrorKind::ProviderFailure,
                    ));
                }
                Ok(Embedding {
                    vectors: vec![vec![1.0, 0.0]],
                    input_tokens: 1,
                })
            })
        }));
    app.application.start_mind_worker();
    app.application.notify_mind_work();
    let wait_for = |calls: Arc<AtomicUsize>, expected: usize| async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while calls.load(Ordering::SeqCst) < expected && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        calls.load(Ordering::SeqCst)
    };
    assert_eq!(wait_for(calls.clone(), 1).await, 1);
    for expected in 2..=5 {
        set_test_now(1_800_000_000 + expected as u64 * 1_000);
        app.application.notify_mind_work();
        assert_eq!(wait_for(calls.clone(), expected).await, expected);
    }
    // The fifth attempt is the budget limit; later time and self-notifies
    // must not call the provider again.
    set_test_now(1_800_000_000 + 60_000);
    app.application.notify_mind_work();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 5);

    fail.store(false, Ordering::SeqCst);
    store
        .mind_retry_index(crate::application::mind::now())
        .await
        .unwrap();
    app.application.notify_mind_work();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while calls.load(Ordering::SeqCst) < 6 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "retry reset did not resume"
    );
    assert_eq!(
        store
            .mind_index_status(companion.clone(), "2026-01-01T00:00:00Z".into())
            .await
            .unwrap()
            .pending_vectors,
        0
    );
    set_test_now(0);
    app.application.stop().await.unwrap();
}
