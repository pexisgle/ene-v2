mod endpoint_tests;

use super::*;
use crate::host::{
    db::Store,
    error::HostError,
    mind::{Companion, ModelAssignment, ModelAssignments, Profile, Role, State},
    model::{self, Credentials},
    settings::{
        Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        SttConnection, TtsConnection,
    },
};
use ene_mind::JsonDoc;
use parking_lot::Mutex;
use rig_core::catalog::Catalog;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct NativeEndpoint {
    pub(super) base: String,
    pub(super) requests: Arc<AtomicUsize>,
    pub(super) headers: Arc<Mutex<Vec<axum::http::HeaderMap>>>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl NativeEndpoint {
    pub(super) async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let headers = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&headers);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            count.fetch_add(1, Ordering::SeqCst);
            observed.lock().push(headers);
            async move {
                let body = match uri.path() {
                    "/audio/speech" => vec![7, 0],
                    "/audio/transcriptions" => br#"{"text":"native transcript"}"#.to_vec(),
                    "/models" => br#"{"data":[{"id":"native-model"}]}"#.to_vec(),
                    _ => br#"{"id":"native","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"native completion"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#.to_vec(),
                };
                ([
                    ("content-type", "application/json"),
                    ("x-safe", "visible"),
                    ("set-cookie", "private=value"),
                    ("authorization", "private-response"),
                ], body)
            }
        });
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            requests,
            headers,
            task,
        }
    }

    pub(super) fn assert_requests(&self, expected: usize) {
        assert_eq!(self.requests.load(Ordering::SeqCst), expected);
    }
}

impl Drop for NativeEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn connection(kind: ConnectionKind, base: &str) -> Connection {
    let endpoint = Endpoint {
        adapter: "openrouter".into(),
        endpoint: base.into(),
        options: "{}".into(),
        ..Default::default()
    };
    let config = match kind {
        ConnectionKind::Llm => ConnectionConfig::Llm(LlmConnection {
            endpoint,
            model_ids: vec!["m".into()],
        }),
        ConnectionKind::Tts => ConnectionConfig::Tts(TtsConnection {
            endpoint,
            voice_ids: vec![],
        }),
        ConnectionKind::Stt => ConnectionConfig::Stt(SttConnection {
            endpoint,
            language_hints: vec![],
        }),
        _ => unreachable!(),
    };
    Connection {
        id: ene_domain::ConnectionId::parse("p08").unwrap(),
        name: "P08".into(),
        enabled: true,
        kind,
        config,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    }
}

pub(super) fn store(root: &Path, connection: &Connection) -> Store {
    let store = Store::open(&root.join("host.sqlite3")).unwrap();
    let revision = store.settings_repo().settings_revision().unwrap();
    store
        .settings_repo()
        .update_settings(revision, &[(connection.clone(), 0)])
        .unwrap();
    store
}

pub(super) fn companion() -> Companion {
    Companion {
        id: "p08".into(),
        name: "P08".into(),
        mind: String::new(),
        profile: Profile::default(),
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::empty(),
        voice: JsonDoc::empty(),
        assignments: ModelAssignments {
            companion: ModelAssignment {
                connection_id: "p08".into(),
                model_id: "m".into(),
            },
            ..Default::default()
        },
        persona: None,
        voice_identifier: None,
        revision: 1,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

pub(super) fn guest_write(data: &str) -> String {
    let mut body = format!("i64.const {} call $alloc local.set $p ", data.len());
    for (offset, byte) in data.bytes().enumerate() {
        body += &format!("local.get $p i64.const {offset} i64.add i32.const {byte} call $store ");
    }
    body
}

// Extism's enabled Wasmtime `wat` feature accepts this textual module directly.
pub(super) async fn providers(
    root: &Path,
    extension: Extension,
    body: &str,
    destination: &str,
) -> Providers {
    let providers =
        installed_providers(root, extension, body, destination, &json!({"type":"none"}));
    providers.start("openrouter", extension).await.unwrap();
    providers
}

pub(super) fn installed_providers(
    root: &Path,
    extension: Extension,
    body: &str,
    destination: &str,
    auth: &serde_json::Value,
) -> Providers {
    let path = root.join(PLUGINS_DIR).join("openrouter");
    let capabilities =
        r#"{"streaming_text":true,"model_list":true,"tool_calls":true,"openai_http":true}"#;
    let wasm = format!(
        r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/env" "length" (func $length (param i64) (result i64)))
        (import "extism:host/env" "load_u8" (func $load (param i64) (result i32)))
        (import "extism:host/env" "error_set" (func $error (param i64)))
        (import "extism:host/env" "input_length" (func $input_length (result i64)))
        (import "extism:host/env" "input_load_u8" (func $input_byte (param i64) (result i32)))
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (global $calls (mut i32) (i32.const 0))
        (func (export "ene_initialize") (result i32) (local $p i64)
            {} local.get $p i64.const {} call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) (local $i i64)
            global.get $calls i32.const 1 i32.add global.set $calls
            {body} i32.const 0))"#,
        guest_write(capabilities),
        capabilities.len()
    );
    super::tests::write(path.join("main.wasm"), wasm);
    super::tests::write(
        path.join("manifest.json"),
        json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "openrouter", "name": "P08 fixture", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": [extension], "uses": ["http"],
                "provider": {
                    "id": "p08-plugin", "endpoint": destination,
                    "auth": auth, "destinations": [destination],
                    "environment_variable": "ENE_P08_KEY",
                    "options": {"fixture_option": "preserved"}
                }
            }]
        })
        .to_string(),
    );
    publish_openrouter_fixture(root)
}

fn publish_openrouter_fixture(root: &Path) -> Providers {
    let store = Store::open(&root.join("host.sqlite3")).unwrap();
    install_local_package(&store, root, &root.join(PLUGINS_DIR).join("openrouter")).unwrap();
    Providers::load(root, &store).unwrap()
}

pub(super) fn reply(document: &str) -> String {
    format!(
        "{} local.get $p i64.const {} call $output",
        guest_write(document),
        document.len()
    )
}

pub(super) fn emit(document: &str) -> String {
    format!("{} local.get $p call $emit", guest_write(document))
}

pub(super) fn service(document: &serde_json::Value) -> String {
    format!(
        "{} local.get $p call $service local.set $p",
        guest_write(&document.to_string())
    )
}

pub(super) fn assert_guest_bytes(needle: &str, input: bool, present: bool) -> String {
    let length = if input {
        "call $input_length"
    } else {
        "local.get $p call $length"
    };
    let mut matches = String::new();
    for (offset, byte) in needle.bytes().enumerate() {
        matches += &if input {
            format!("local.get $i i64.const {offset} i64.add call $input_byte ")
        } else {
            format!("local.get $p local.get $i i64.add i64.const {offset} i64.add call $load ")
        };
        matches += &format!("i32.const {byte} i32.eq ");
        if offset > 0 {
            matches += "i32.and ";
        }
    }
    let (missing, found) = if present {
        ("unreachable", "br $found")
    } else {
        ("br $found", "unreachable")
    };
    format!(
        "block $found i64.const 0 local.set $i loop $scan
            local.get $i i64.const {} i64.add {length} i64.gt_u if {missing} end
            {matches} if {found} end
            local.get $i i64.const 1 i64.add local.set $i br $scan
        end end",
        needle.len()
    )
}

#[tokio::test(flavor = "current_thread")]
async fn p08_completion_construction_failure_never_selects_native() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let providers = providers(root.path(), Extension::Llm, &reply("{}"), &native.base).await;
    let _failure = construction_failure::Guard::enter();
    let result = model::connect(
        &store,
        &Credentials::new(true),
        Catalog::builtin(),
        &companion(),
        Role::Companion,
        &providers,
    );
    let Err(HostError::Unavailable(message)) = result else {
        panic!("expected unavailable transport error")
    };
    assert!(message.starts_with("plugin HTTP transport: "), "{message}");
    native.assert_requests(0);
}

#[tokio::test(flavor = "current_thread")]
async fn p08_listing_construction_failure_never_selects_native() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let providers = providers(root.path(), Extension::Llm, &reply("{}"), &native.base).await;
    let _failure = construction_failure::Guard::enter();
    let check = model::probe(&Credentials::new(true), &connection, &providers).await;
    assert!(!check.healthy);
    let failure = check.failure.unwrap();
    assert_eq!(failure.code, "connection_check");
    assert!(
        failure.message.starts_with("plugin HTTP transport: "),
        "{}",
        failure.message
    );
    native.assert_requests(0);
}

#[tokio::test]
async fn p10_listing_rejects_missing_or_failed_results() {
    let mut incorrectly_healthy = Vec::new();
    for (case, body) in [
        ("missing_models", reply("{}")),
        ("trap", "unreachable".to_owned()),
        (
            "models_then_trap",
            format!(
                "{} unreachable",
                emit(r#"{"type":"models","models":[{"id":"partial","name":"Partial"}]}"#)
            ),
        ),
    ] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let providers = providers(root.path(), Extension::Llm, &body, &native.base).await;
        let check = model::probe(&Credentials::new(true), &connection, &providers).await;
        if check.healthy {
            incorrectly_healthy.push(case);
        } else {
            assert_eq!(check.failure.unwrap().code, "connection_check", "{case}");
        }
        native.assert_requests(0);
    }
    assert!(
        incorrectly_healthy.is_empty(),
        "failed listing reported healthy: {incorrectly_healthy:?}"
    );
}

#[tokio::test]
async fn p10_listing_starts_installed_plugins_and_accepts_empty_models() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let providers = installed_providers(
        root.path(),
        Extension::Llm,
        &format!(
            "{} {}",
            emit(r#"{"type":"models","models":[]}"#),
            reply("{}")
        ),
        &native.base,
        &json!({"type":"none"}),
    );
    assert!(providers.running("openrouter", Extension::Llm).is_none());
    for _ in 0..2 {
        let check = model::probe(&Credentials::new(true), &connection, &providers).await;
        assert!(check.healthy, "{:?}", check.failure);
        assert_eq!(check.resources, ["m"]);
    }
    assert!(providers.running("openrouter", Extension::Llm).is_some());
    native.assert_requests(0);
}

#[test]
fn p10_concurrent_cold_probes_share_startup_without_admission_failures() {
    use std::{future::Future, task::Poll, time::Duration};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let credentials = Credentials::new(true);
        let providers = installed_providers(
            root.path(),
            Extension::Llm,
            &format!(
                "{} {}",
                emit(
                    r#"{"type":"models","models":[{"id":"shared-plugin-model","name":"Shared"}]}"#
                ),
                reply("{}")
            ),
            &native.base,
            &json!({"type":"none"}),
        );
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _discarded = entered.send(());
            let _discarded = blocked.recv();
        });
        ready.await.unwrap();
        let mut probes: Vec<_> = (0..17)
            .map(|_| Box::pin(model::probe(&credentials, &connection, &providers)))
            .collect();
        let mut checks: Vec<_> = (0..probes.len()).map(|_| None).collect();
        std::future::poll_fn(|cx| {
            for (probe, check) in probes.iter_mut().zip(&mut checks) {
                if let Poll::Ready(result) = probe.as_mut().poll(cx) {
                    *check = Some(result);
                }
            }
            Poll::Ready(())
        })
        .await;
        assert!(providers.running("openrouter", Extension::Llm).is_none());
        drop(release);
        blocker.await.unwrap();
        let checks = tokio::time::timeout(Duration::from_secs(10), async {
            let mut completed = Vec::new();
            for (probe, check) in probes.into_iter().zip(checks) {
                completed.push(match check {
                    Some(check) => check,
                    None => probe.await,
                });
            }
            completed
        })
        .await
        .expect("concurrent cold probes did not finish");
        let failures: Vec<_> = checks
            .iter()
            .enumerate()
            .filter(|(_, check)| !check.healthy)
            .map(|(index, check)| (index, &check.failure))
            .collect();
        native.assert_requests(0);
        assert!(failures.is_empty(), "cold probe failures: {failures:?}");
        for check in checks {
            assert!(check.resources.contains(&"shared-plugin-model".to_owned()));
            assert!(!check.resources.contains(&"native-model".to_owned()));
        }
    });
}

#[test]
fn p10_cold_probe_deadline_includes_queued_startup_and_allows_retry() {
    use std::time::{Duration, Instant};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let credentials = Credentials::new(true);
        let providers = installed_providers(
            root.path(),
            Extension::Llm,
            &format!(
                "{} {}",
                emit(r#"{"type":"models","models":[]}"#),
                reply("{}")
            ),
            &native.base,
            &json!({"type":"none"}),
        );
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _discarded = entered.send(());
            let _discarded = blocked.recv();
        });
        ready.await.unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(18),
            model::probe(&credentials, &connection, &providers),
        )
        .await;
        let elapsed = started.elapsed();
        drop(release);
        blocker.await.unwrap();
        let check = result.expect("cold startup escaped the 15-second probe deadline");
        assert!(!check.healthy);
        assert_eq!(
            check.failure.unwrap().message,
            "the plugin did not answer in 15s"
        );
        assert!(
            elapsed >= Duration::from_secs(15),
            "deadline fired early: {elapsed:?}"
        );
        assert!(providers.running("openrouter", Extension::Llm).is_none());
        let retry = tokio::time::timeout(
            Duration::from_secs(5),
            model::probe(&credentials, &connection, &providers),
        )
        .await
        .expect("cancelled startup poisoned the next probe");
        assert!(retry.healthy, "{:?}", retry.failure);
        native.assert_requests(0);
    });
}

#[tokio::test]
async fn p10_listing_initialization_failure_never_selects_native() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    installed_providers(
        root.path(),
        Extension::Llm,
        &reply("{}"),
        &native.base,
        &json!({"type":"none"}),
    );
    let path = root.path().join(PLUGINS_DIR).join("openrouter/main.wasm");
    let wasm = std::fs::read_to_string(&path).unwrap().replace(
        "(func (export \"ene_initialize\") (result i32) (local $p i64)",
        "(func (export \"ene_initialize\") (result i32) (local $p i64) unreachable",
    );
    super::tests::write(path, wasm);
    let providers = publish_openrouter_fixture(root.path());
    assert!(providers.get("openrouter").is_some());
    let check = model::probe(&Credentials::new(true), &connection, &providers).await;
    assert!(!check.healthy);
    let failure = check.failure.unwrap();
    assert_eq!(failure.code, "connection_check");
    assert!(
        failure.message.contains("plugin load:"),
        "{}",
        failure.message
    );
    assert!(providers.running("openrouter", Extension::Llm).is_none());
    native.assert_requests(0);
}
