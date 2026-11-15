use super::*;
use crate::host::{
    db::Store,
    error::HostError,
    json::JsonDoc,
    mind::{Assignment, Assignments, Companion, Profile, Role, State},
    model::{self, Credentials},
    settings::{
        Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        SttConnection, TtsConnection,
    },
    voice::{self, SetupError},
};
use bevy_ecs::world::World;
use rig_core::{catalog::Catalog, completion::CompletionRequest};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NativeEndpoint {
    base: String,
    requests: Arc<AtomicUsize>,
    headers: Arc<Mutex<Vec<axum::http::HeaderMap>>>,
    task: tokio::task::JoinHandle<()>,
}

impl NativeEndpoint {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let headers = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&headers);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
            count.fetch_add(1, Ordering::SeqCst);
            observed.lock().unwrap().push(headers);
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

    fn assert_requests(&self, expected: usize) {
        assert_eq!(self.requests.load(Ordering::SeqCst), expected);
    }
}

impl Drop for NativeEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn connection(kind: ConnectionKind, base: &str) -> Connection {
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
        id: "p08".into(),
        name: "P08".into(),
        enabled: true,
        kind,
        config,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    }
}

fn store(root: &Path, connection: &Connection) -> Store {
    let store = Store::open(&root.join("host.sqlite3")).unwrap();
    let revision = store.settings_revision().unwrap();
    store
        .update_settings(revision, &[(connection.clone(), 0)], &[])
        .unwrap();
    store
}

fn companion() -> Companion {
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
        assignments: Assignments {
            companion: Assignment {
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

fn guest_write(data: &str) -> String {
    let mut body = format!("i64.const {} call $alloc local.set $p ", data.len());
    for (offset, byte) in data.bytes().enumerate() {
        body += &format!("local.get $p i64.const {offset} i64.add i32.const {byte} call $store ");
    }
    body
}

// Extism's enabled Wasmtime `wat` feature accepts this textual module directly.
async fn providers(root: &Path, extension: Extension, body: &str, destination: &str) -> Providers {
    let providers = installed_providers(root, extension, body, destination, json!({"type":"none"}));
    providers.start("openrouter", extension).await.unwrap();
    providers
}

fn installed_providers(
    root: &Path,
    extension: Extension,
    body: &str,
    destination: &str,
    auth: serde_json::Value,
) -> Providers {
    let path = root.join(PLUGINS_DIR).join("openrouter");
    let capabilities = r#"{"streaming_text":true,"model_list":true,"tool_calls":true}"#;
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
                    "options": {"fixture_option": "preserved"}
                }
            }]
        })
        .to_string(),
    );
    Providers::load(root)
}

fn reply(document: &str) -> String {
    format!(
        "{} local.get $p i64.const {} call $output",
        guest_write(document),
        document.len()
    )
}

fn emit(document: &str) -> String {
    format!("{} local.get $p call $emit", guest_write(document))
}

fn service(document: serde_json::Value) -> String {
    format!(
        "{} local.get $p call $service local.set $p",
        guest_write(&document.to_string())
    )
}

fn assert_guest_bytes(needle: &str, input: bool, present: bool) -> String {
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

fn observe_request() -> CompletionRequest {
    use rig_core::completion::{ToolDefinition, message::ToolName};
    CompletionRequest::new("hello").tools([ToolDefinition::new(
        ToolName::new("observe").unwrap(),
        "Observe the actual guest input",
        json!({"type":"object"}),
    )])
}

fn observe_input() -> String {
    let prefix = r#"{"type":"tool_call","id":"observed","name":"observe","arguments":{"calls":0,"operation":"#;
    let suffix = "}}";
    let mut body = format!(
        "call $input_length i64.const {} i64.add call $alloc local.set $p ",
        prefix.len() + suffix.len()
    );
    for (offset, byte) in prefix.bytes().enumerate() {
        body += &format!("local.get $p i64.const {offset} i64.add i32.const {byte} call $store ");
    }
    let calls_offset = prefix.find("0,").unwrap();
    body += &format!(
        "local.get $p i64.const {calls_offset} i64.add global.get $calls i32.const 48 i32.add call $store
         i64.const 0 local.set $i
         block $done loop $copy
             local.get $i call $input_length i64.ge_u br_if $done
             local.get $p i64.const {} i64.add local.get $i i64.add
             local.get $i call $input_byte call $store
             local.get $i i64.const 1 i64.add local.set $i br $copy
         end end ",
        prefix.len()
    );
    for (offset, byte) in suffix.bytes().enumerate() {
        body += &format!(
            "local.get $p call $input_length i64.add i64.const {} i64.add i32.const {byte} call $store ",
            prefix.len() + offset
        );
    }
    body += "local.get $p call $emit ";
    body += &format!(
        "{} {}",
        emit(r#"{"type":"finished","reason":"tool_calls"}"#),
        reply("{}")
    );
    body
}

#[tokio::test]
async fn p08_plugin_observes_resolved_model_and_preserves_request() {
    use rig_core::completion::message::ToolChoice;
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let providers = providers(root.path(), Extension::Llm, &observe_input(), &native.base).await;
    let model = model::connect(
        &store,
        &Credentials::new(true),
        Catalog::builtin(),
        &companion(),
        Role::Companion,
        &providers,
    )
    .unwrap();
    for (index, (requested, expected)) in [
        (
            Some("vendor/family/model:revision"),
            "vendor/family/model:revision",
        ),
        (None, "m"),
        (Some(""), "m"),
    ]
    .into_iter()
    .enumerate()
    {
        let response = model
            .call(
                observe_request()
                    .model::<&str>(requested)
                    .tool_choice(ToolChoice::Auto)
                    .max_tokens(37),
            )
            .await
            .unwrap();
        let observed = &response.tool_calls().next().unwrap().function.arguments;
        assert_eq!(observed["calls"], index + 1);
        let operation = &observed["operation"];
        assert_eq!(operation["type"], "generate");
        let request = &operation["request"];
        assert_eq!(request["model"], expected);
        assert_eq!(request["endpoint"], native.base);
        assert_eq!(request["options"], json!({"fixture_option":"preserved"}));
        assert_eq!(request["max_tokens"], 37);
        assert_eq!(request["tools"][0]["name"], "observe");
        assert_eq!(
            request["messages"][0],
            json!({"kind":"text","role":"user","text":"hello"})
        );
    }
    let first = model.call(observe_request()).await.unwrap();
    let call = first.tool_calls().next().unwrap();
    let result = rig_core::completion::Message::tool_result(
        call.id.clone(),
        call.function.name.clone(),
        "tool output",
    );
    let second = model
        .call(
            observe_request()
                .messages([
                    rig_core::completion::Message::user("hello"),
                    first.message().unwrap(),
                    result,
                ])
                .preamble("system instructions"),
        )
        .await
        .unwrap();
    let observed = &second.tool_calls().next().unwrap().function.arguments;
    assert_eq!(observed["calls"], 5);
    let request = &observed["operation"]["request"];
    assert_eq!(request["model"], "m");
    assert_eq!(request["max_tokens"], 4096);
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({"kind":"text","role":"system","text":"system instructions"})
    );
    assert!(messages.contains(&json!({
        "kind":"tool_calls", "calls":[{
            "id":"observed", "name":"observe", "arguments":call.function.arguments
        }]
    })));
    assert!(messages.contains(&json!({
        "kind":"tool_result", "call_id":"observed", "name":"observe",
        "content":"tool output", "is_error":false
    })));
    native.assert_requests(0);
}

#[tokio::test]
async fn p08_plugin_rejects_unrepresentable_controls_before_invocation() {
    use rig_core::completion::message::{ToolChoice, ToolName};
    let native = NativeEndpoint::start().await;
    let upstream = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let open = json!({"type":"http_open","request":{
        "method":"GET", "url":upstream.base, "headers":{}
    }})
    .to_string();
    let body = format!(
        "{} local.get $p call $service drop {}",
        guest_write(&open),
        observe_input()
    );
    let providers = providers(root.path(), Extension::Llm, &body, &upstream.base).await;
    let model = model::connect(
        &store,
        &Credentials::new(true),
        Catalog::builtin(),
        &companion(),
        Role::Companion,
        &providers,
    )
    .unwrap();
    let mut additional = observe_request();
    additional.additional_params = Some(json!({"custom":true}));
    let mut empty_additional = observe_request();
    empty_additional.additional_params = Some(json!({}));
    let mut provider = observe_request();
    provider.provider_options = serde_json::from_value(json!({
        "p08-plugin":{"body":{"custom":true}}
    }))
    .unwrap();
    let schema: schemars::Schema = serde_json::from_value(json!({"type":"object"})).unwrap();
    let mut silently_accepted = Vec::new();
    for (name, request) in [
        ("temperature", observe_request().temperature(0.0)),
        ("temperature", observe_request().temperature(0.7)),
        (
            "tool_choice",
            observe_request().tool_choice(ToolChoice::None),
        ),
        (
            "tool_choice",
            observe_request().tool_choice(ToolChoice::Required),
        ),
        (
            "tool_choice",
            observe_request().tool_choice(ToolChoice::Specific {
                function_names: vec![ToolName::new("observe").unwrap()],
            }),
        ),
        ("output_schema", observe_request().output_schema(schema)),
        ("additional_params", additional),
        ("additional_params", empty_additional),
        ("provider_options", provider),
        ("seed", observe_request().seed(7)),
    ] {
        let error = match model.call(request).await {
            Ok(_) => {
                silently_accepted.push(name);
                continue;
            }
            Err(error) => error,
        };
        assert!(error.to_string().contains(name), "{name}: {error}");
        assert!(
            error.to_string().contains("plugin generation contract"),
            "{name}: {error}"
        );
    }
    assert!(
        silently_accepted.is_empty(),
        "silently accepted {silently_accepted:?}"
    );
    upstream.assert_requests(0);
    native.assert_requests(0);
    for calls in 1..=2 {
        let response = model.call(observe_request()).await.unwrap();
        let observed = &response.tool_calls().next().unwrap().function.arguments;
        assert_eq!(
            observed["calls"], calls,
            "rejected requests must not invoke the guest"
        );
        upstream.assert_requests(calls as usize);
        native.assert_requests(0);
    }
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
    let store = store(root.path(), &connection);
    let providers = providers(root.path(), Extension::Llm, &reply("{}"), &native.base).await;
    let _failure = construction_failure::Guard::enter();
    let check = model::probe(&store, &Credentials::new(true), &connection, &providers).await;
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
        let store = store(root.path(), &connection);
        let providers = providers(root.path(), Extension::Llm, &body, &native.base).await;
        let check = model::probe(&store, &Credentials::new(true), &connection, &providers).await;
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
    let store = store(root.path(), &connection);
    let providers = installed_providers(
        root.path(),
        Extension::Llm,
        &format!(
            "{} {}",
            emit(r#"{"type":"models","models":[]}"#),
            reply("{}")
        ),
        &native.base,
        json!({"type":"none"}),
    );
    assert!(providers.running("openrouter", Extension::Llm).is_none());
    for _ in 0..2 {
        let check = model::probe(&store, &Credentials::new(true), &connection, &providers).await;
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
        let store = store(root.path(), &connection);
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
            json!({"type":"none"}),
        );
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            let _ = blocked.recv();
        });
        ready.await.unwrap();
        let mut probes: Vec<_> = (0..17)
            .map(|_| Box::pin(model::probe(&store, &credentials, &connection, &providers)))
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
        let store = store(root.path(), &connection);
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
            json!({"type":"none"}),
        );
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            let _ = blocked.recv();
        });
        ready.await.unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(18),
            model::probe(&store, &credentials, &connection, &providers),
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
            model::probe(&store, &credentials, &connection, &providers),
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
    let store = store(root.path(), &connection);
    installed_providers(
        root.path(),
        Extension::Llm,
        &reply("{}"),
        &native.base,
        json!({"type":"none"}),
    );
    let path = root.path().join(PLUGINS_DIR).join("openrouter/main.wasm");
    let wasm = std::fs::read_to_string(&path).unwrap().replace(
        "(func (export \"ene_initialize\") (result i32) (local $p i64)",
        "(func (export \"ene_initialize\") (result i32) (local $p i64) unreachable",
    );
    super::tests::write(path, wasm);
    let providers = Providers::load(root.path());
    assert!(providers.get("openrouter").is_some());
    let check = model::probe(&store, &Credentials::new(true), &connection, &providers).await;
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

#[tokio::test]
async fn p10_listing_uses_authorized_http_and_connection_credentials() {
    use base64::Engine;
    let native = NativeEndpoint::start().await;
    let upstream = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let secret = "listing-host-secret";
    let credentials = Credentials::new(true);
    credentials.remember("another-connection", "wrong-secret".into());
    credentials.remember(&connection.id, secret.into());
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(br#"{"data":[{"id":"native-model"}]}"#);
    let body = [
        assert_guest_bytes(&format!("\"endpoint\":\"{}\"", native.base), true, true),
        assert_guest_bytes(r#""options":{"fixture_option":"preserved"}"#, true, true),
        assert_guest_bytes(secret, true, false),
        assert_guest_bytes("replacement-secret", true, false),
        service(json!({"type":"http_open", "request":{
            "method":"GET", "url":format!("{}/models", upstream.base), "headers":{}
        }})),
        assert_guest_bytes(r#""Ok":{"#, false, true),
        service(json!({"type":"http_read", "handle":"http-1"})),
        assert_guest_bytes(&format!("\"data_base64\":\"{encoded}\""), false, true),
        assert_guest_bytes(r#""done":true"#, false, true),
        assert_guest_bytes(secret, false, false),
        assert_guest_bytes("replacement-secret", false, false),
        assert_guest_bytes("private-response", false, false),
        assert_guest_bytes("set-cookie", false, false),
        service(json!({"type":"http_close", "handle":"http-1"})),
        assert_guest_bytes(r#"{"Ok":null}"#, false, true),
        emit(r#"{"type":"models","models":[{"id":"plugin-http-model","name":"HTTP"}]}"#),
        reply("{}"),
    ]
    .join(" ");
    let providers = installed_providers(
        root.path(),
        Extension::Llm,
        &body,
        &upstream.base,
        json!({"type":"bearer"}),
    );
    for key in [secret, "replacement-secret"] {
        credentials.remember(&connection.id, key.into());
        let check = model::probe(&store, &credentials, &connection, &providers).await;
        assert!(check.healthy, "{:?}", check.failure);
        assert!(check.resources.contains(&"plugin-http-model".into()));
        assert!(!check.resources.contains(&"native-model".into()));
        assert_eq!(
            upstream.headers.lock().unwrap().last().unwrap()["authorization"]
                .to_str()
                .unwrap(),
            format!("Bearer {key}")
        );
    }
    upstream.assert_requests(2);
    native.assert_requests(0);
}

#[tokio::test]
async fn p10_listing_http_failures_never_select_native_or_expand_origins() {
    for denied in [true, false] {
        let native = NativeEndpoint::start().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let approved = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let store = store(root.path(), &connection);
        let destination = if denied { &native.base } else { &approved };
        let body = format!(
            "{} local.get $p call $error i32.const 1 return",
            service(json!({"type":"http_open", "request":{
                "method":"GET", "url":destination, "headers":{}
            }}))
        );
        let providers = providers(root.path(), Extension::Llm, &body, &approved).await;
        let check = model::probe(&store, &Credentials::new(true), &connection, &providers).await;
        assert!(!check.healthy);
        let failure = check.failure.unwrap();
        assert_eq!(failure.code, "connection_check");
        let code = if denied {
            "destination_denied"
        } else {
            "http_failed"
        };
        assert!(failure.message.contains(code), "{}", failure.message);
        native.assert_requests(0);
    }
}

#[tokio::test]
async fn p10_listing_timeout_and_caller_drop_close_http() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for timeout in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &base);
        let body = format!(
            "{} {} {}",
            service(json!({
                "type":"http_open", "request":{"method":"GET", "url":base, "headers":{}}
            })),
            emit(r#"{"type":"models","models":[]}"#),
            reply("{}")
        );
        let providers = providers(root.path(), Extension::Llm, &body, &base).await;
        let store = store(root.path(), &connection);
        let credentials = Credentials::new(true);
        let mut run = Box::pin(model::probe(&store, &credentials, &connection, &providers));
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut run => panic!("listing returned before HTTP started: {result:?}"),
                result = listener.accept() => result.unwrap(),
            }
        })
        .await
        .unwrap();
        let mut request = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&request).starts_with("GET / HTTP/1.1"));
        if timeout {
            let check = run.await;
            assert!(!check.healthy);
            assert_eq!(
                check.failure.unwrap().message,
                "the plugin did not answer in 15s"
            );
        } else {
            drop(run);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut byte = [0];
            assert_eq!(
                socket.read(&mut byte).await.unwrap(),
                0,
                "broker socket outlived listing"
            );
        })
        .await
        .expect("HTTP socket outlived the listing check");
        let _ = socket.shutdown().await;
        let check = tokio::time::timeout(
            Duration::from_secs(2),
            model::probe(&store, &credentials, &connection, &providers),
        )
        .await
        .expect("the cancelled invocation kept the provider occupied");
        assert!(!check.healthy);
        assert!(check.failure.unwrap().message.contains("unavailable"));
    }
}

#[tokio::test]
async fn p10_bounded_stream_drains_beyond_capacity_and_closes_after_guest_success() {
    use ene_plugin::{Event, Operation};
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let body = format!(
        "{} {} {}",
        format!("{} ", emit(r#"{"type":"delta","text":"chunk"}"#)).repeat(64),
        emit(r#"{"type":"finished","reason":"stop"}"#),
        reply("{}")
    );
    let providers = providers(root.path(), Extension::Llm, &body, "https://unused.example").await;
    let running = providers.running("openrouter", Extension::Llm).unwrap();
    let broker = http_broker(Arc::new(ene_plugin_runtime::service::DenyHttp)).unwrap();
    let native = Arc::downgrade(&broker);
    let services = BrokerServices::new(broker, running.provider().id.clone());
    let (sender, mut events) = tokio::sync::mpsc::channel(16);
    assert_eq!(sender.max_capacity(), 16);
    let operation = Operation::Generate {
        request: serde_json::from_value(json!({
            "endpoint":"https://unused.example", "options":{}, "model":"m",
            "preamble":"", "messages":[], "tools":[], "max_tokens":1
        }))
        .unwrap(),
    };
    let (result, collected) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            running.invoke_streaming(operation, services, sender),
            async move {
                let mut collected = Vec::new();
                while let Some(event) = events.recv().await {
                    collected.push(event);
                }
                collected
            }
        )
    })
    .await
    .expect("bounded stream failed to drain or its channel never closed");
    result.unwrap();
    assert_eq!(collected.len(), 65);
    assert!(
        collected[..64]
            .iter()
            .all(|event| matches!(event, Event::Delta { text } if text == "chunk"))
    );
    assert!(matches!(&collected[64], Event::Finished { reason } if reason == "stop"));
    assert!(native.upgrade().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn p08_speech_construction_failure_never_selects_native() {
    for (kind, extension) in [
        (ConnectionKind::Tts, Extension::Tts),
        (ConnectionKind::Stt, Extension::Stt),
    ] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(kind, &native.base);
        let store = store(root.path(), &connection);
        let providers = providers(root.path(), extension, &reply("{}"), &native.base).await;
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(providers);
        let _failure = construction_failure::Guard::enter();
        let error = if kind == ConnectionKind::Tts {
            voice::tts_source(&world).err().expect("TTS must fail")
        } else {
            voice::stt_source(&world).err().expect("STT must fail")
        };
        assert!(
            matches!(error, SetupError::Broken(ref message) if message.starts_with("plugin HTTP transport: ")),
            "{error:?}"
        );
        native.assert_requests(0);
    }
}

#[tokio::test]
async fn p08_plugin_completion_and_listing_win_over_native() {
    for listing in [false, true] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let store = store(root.path(), &connection);
        let events = if listing {
            emit(r#"{"type":"models","models":[{"id":"plugin-model","name":"Plugin"}]}"#)
        } else {
            format!(
                "{} {}",
                emit(r#"{"type":"delta","text":"plugin completion"}"#),
                emit(r#"{"type":"finished","reason":"stop"}"#)
            )
        };
        let providers = providers(
            root.path(),
            Extension::Llm,
            &format!("{events} {}", reply("{}")),
            &native.base,
        )
        .await;
        let credentials = Credentials::new(true);
        if listing {
            let check = model::probe(&store, &credentials, &connection, &providers).await;
            assert!(check.healthy, "{:?}", check.failure);
            assert!(check.resources.contains(&"plugin-model".into()));
            assert!(!check.resources.contains(&"native-model".into()));
        } else {
            let model = model::connect(
                &store,
                &credentials,
                Catalog::builtin(),
                &companion(),
                Role::Companion,
                &providers,
            )
            .unwrap();
            let error = model
                .call(CompletionRequest::new("hello").seed(7))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("seed"), "{error}");
            assert!(
                error.to_string().contains("plugin generation contract"),
                "{error}"
            );
            for _ in 0..2 {
                assert_eq!(
                    model
                        .call(CompletionRequest::new("hello"))
                        .await
                        .unwrap()
                        .text(),
                    "plugin completion"
                );
            }
        }
        native.assert_requests(0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p08_absent_plugin_keeps_native_completion_and_listing() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, &connection.id, "test-only").unwrap();
    let _failure = construction_failure::Guard::enter();
    let providers = Providers::none();
    let model = model::connect(
        &store,
        &credentials,
        Catalog::builtin(),
        &companion(),
        Role::Companion,
        &providers,
    )
    .unwrap();
    assert_eq!(
        model
            .call(CompletionRequest::new("hello"))
            .await
            .unwrap()
            .text(),
        "native completion"
    );
    let check = model::probe(&store, &credentials, &connection, &providers).await;
    assert!(check.healthy, "{:?}", check.failure);
    assert!(check.resources.contains(&"native-model".into()));
    native.assert_requests(2);
}

#[tokio::test(flavor = "current_thread")]
async fn p08_absent_plugin_keeps_native_speech() {
    for kind in [ConnectionKind::Tts, ConnectionKind::Stt] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(kind, &native.base);
        let mut world = World::new();
        world.insert_resource(store(root.path(), &connection));
        world.insert_resource(Providers::none());
        let _failure = construction_failure::Guard::enter();
        if kind == ConnectionKind::Tts {
            assert_eq!(
                voice::tts_source(&world)
                    .unwrap()
                    .synthesize("hello".into(), "v".into())
                    .await
                    .unwrap()
                    .0,
                [7, 0]
            );
        } else {
            assert_eq!(
                voice::stt_source(&world)
                    .unwrap()
                    .transcribe(vec![0, 0], 24_000)
                    .await
                    .unwrap(),
                "native transcript"
            );
        }
        native.assert_requests(1);
    }
}

#[tokio::test]
async fn p08_plugin_tts_wins_over_native() {
    let native = NativeEndpoint::start().await;
    let audio = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Tts, &native.base);
    let open = json!({"type":"http_open","request":{
        "method":"GET", "url":format!("{}/audio/speech", audio.base), "headers":{}
    }})
    .to_string();
    let body = format!(
        "{} local.get $p call $service drop global.get $calls i32.const 1 i32.eq if {} else {} end",
        guest_write(&open),
        reply(r#"{"handle":"http-1","sample_rate":24000,"format":"pcm16","bytes":2}"#),
        reply(r#"{"handle":"http-2","sample_rate":24000,"format":"pcm16","bytes":2}"#)
    );
    let providers = providers(root.path(), Extension::Tts, &body, &audio.base).await;
    let mut world = World::new();
    world.insert_resource(store(root.path(), &connection));
    world.insert_resource(providers);
    let engine = voice::tts_source(&world).unwrap();
    for _ in 0..2 {
        let (pcm, rate) = engine.synthesize("hello".into(), "v".into()).await.unwrap();
        assert_eq!(pcm, [7, 0]);
        assert_eq!(rate, 24_000);
    }
    native.assert_requests(0);
    audio.assert_requests(2);
}

#[test]
fn p08_actual_grant_binds_origins_and_secret_at_construction() {
    use ene_plugin_runtime::service::HttpGrants;
    let connection = connection(ConnectionKind::Llm, "https://connection.example");
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, &connection.id, "host-secret").unwrap();
    let provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":"https://api.example",
        "auth":{"type":"header", "name":"x-api-key"},
        "destinations":["https://api.example/path", "https://api.example:8443", "not a URL"]
    }))
    .unwrap();
    let grant = Grant::new(&provider, &connection, Some(&credentials));
    for provider_argument in ["owner", "another-provider"] {
        for allowed in [
            "https://api.example/other",
            "https://api.example:443",
            "https://api.example:8443/v1",
        ] {
            assert!(grant.allows(provider_argument, &url::Url::parse(allowed).unwrap()));
        }
        for denied in [
            "http://api.example",
            "https://api.example.evil",
            "https://api.example:8444",
            "https://connection.example",
        ] {
            assert!(!grant.allows(provider_argument, &url::Url::parse(denied).unwrap()));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        grant.authorize(provider_argument, &mut headers).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-api-key"], "host-secret");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p08_construction_failure_is_scoped_and_not_an_invalid_destination() {
    use ene_plugin_runtime::service::DenyHttp;
    {
        let _failure = construction_failure::Guard::enter();
        let error = http_broker(Arc::new(DenyHttp))
            .err()
            .expect("builder failure");
        assert!(error.is_builder());
    }
    let broker = http_broker(Arc::new(DenyHttp)).unwrap();
    let invalid =
        serde_json::from_value(json!({"method":"GET","url":"not a URL","headers":{}})).unwrap();
    assert_eq!(
        broker.open("p", invalid).await.unwrap_err().code,
        "invalid_url"
    );
}

#[tokio::test]
async fn p08_plugin_stt_wins_over_native() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Stt, &native.base);
    let providers = providers(
        root.path(),
        Extension::Stt,
        &reply(r#"{"text":"plugin transcript"}"#),
        &native.base,
    )
    .await;
    let mut world = World::new();
    world.insert_resource(store(root.path(), &connection));
    world.insert_resource(providers);
    let engine = voice::stt_source(&world).unwrap();
    for _ in 0..2 {
        assert_eq!(
            engine.transcribe(vec![0, 0], 24_000).await.unwrap(),
            "plugin transcript"
        );
    }
    native.assert_requests(0);
}

#[tokio::test]
async fn p08_plugin_generation_failure_never_selects_native() {
    for terminal in [false, true] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let store = store(root.path(), &connection);
        let body = if terminal {
            format!(
                "{} unreachable",
                emit(r#"{"type":"finished","reason":"stop"}"#)
            )
        } else {
            reply("{}")
        };
        let providers = providers(root.path(), Extension::Llm, &body, &native.base).await;
        let model = model::connect(
            &store,
            &Credentials::new(true),
            Catalog::builtin(),
            &companion(),
            Role::Companion,
            &providers,
        )
        .unwrap();
        let error = model
            .call(CompletionRequest::new("hello"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("plugin call"), "{error}");
        native.assert_requests(0);
    }
}

fn request(url: &str) -> ene_plugin::HttpRequest {
    serde_json::from_value(json!({"method":"GET", "url":url, "headers":{}})).unwrap()
}

#[tokio::test]
async fn p08_actual_grant_enforces_transport_policy_and_snapshots_credentials() {
    let upstream = NativeEndpoint::start().await;
    let denied = NativeEndpoint::start().await;
    let connection = connection(ConnectionKind::Llm, &denied.base);
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, &connection.id, "original-secret").unwrap();
    let provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":upstream.base,
        "auth":{"type":"bearer"}, "destinations":[upstream.base]
    }))
    .unwrap();
    let broker = http_broker(Arc::new(Grant::new(
        &provider,
        &connection,
        Some(&credentials),
    )))
    .unwrap();
    assert_eq!(
        broker
            .open("owner", request(&denied.base))
            .await
            .unwrap_err()
            .code,
        "destination_denied"
    );
    for header in [
        "AuThOrIzAtIoN",
        "pRoXy-AuThOrIzAtIoN",
        "CoOkIe",
        "HoSt",
        "CoNtEnT-LeNgTh",
    ] {
        let mut forbidden = request(&upstream.base);
        forbidden
            .headers
            .insert(header.into(), "guest-value".into());
        assert_eq!(
            broker.open("owner", forbidden).await.unwrap_err().code,
            "reserved_header"
        );
    }
    upstream.assert_requests(0);
    denied.assert_requests(0);
    model::save_secret(&credentials, &connection.id, "replacement-secret").unwrap();
    for provider_argument in ["owner", "foreign-provider"] {
        let opened = broker
            .open(provider_argument, request(&upstream.base))
            .await
            .unwrap();
        let chunk = broker.read(&opened.handle).unwrap();
        assert_eq!(chunk.headers["x-safe"], "visible");
        assert!(!chunk.headers.contains_key("authorization"));
        assert!(!chunk.headers.contains_key("set-cookie"));
        assert_eq!(
            upstream.headers.lock().unwrap().last().unwrap()["authorization"],
            "Bearer original-secret"
        );
    }
    let fresh = http_broker(Arc::new(Grant::new(
        &provider,
        &connection,
        Some(&credentials),
    )))
    .unwrap();
    fresh.open("owner", request(&upstream.base)).await.unwrap();
    assert_eq!(
        upstream.headers.lock().unwrap().last().unwrap()["authorization"],
        "Bearer replacement-secret"
    );
    model::delete_secret(&credentials, &connection.id).unwrap();
    broker.open("owner", request(&upstream.base)).await.unwrap();
    assert_eq!(
        upstream.headers.lock().unwrap().last().unwrap()["authorization"],
        "Bearer original-secret"
    );
    upstream.assert_requests(4);
    denied.assert_requests(0);
}

#[tokio::test]
async fn p08_actual_grant_overwrites_guest_auth_and_refuses_invalid_auth() {
    let upstream = NativeEndpoint::start().await;
    let connection = connection(ConnectionKind::Llm, &upstream.base);
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, &connection.id, "host-secret").unwrap();
    let mut provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":upstream.base,
        "auth":{"type":"header", "name":"x-api-key"}, "destinations":[upstream.base]
    }))
    .unwrap();
    let broker = http_broker(Arc::new(Grant::new(
        &provider,
        &connection,
        Some(&credentials),
    )))
    .unwrap();
    let mut call = request(&upstream.base);
    call.headers
        .insert("X-Api-Key".into(), "guest-secret".into());
    broker.open("foreign-provider", call).await.unwrap();
    assert_eq!(
        upstream.headers.lock().unwrap().last().unwrap()["x-api-key"],
        "host-secret"
    );
    for (auth, expected) in [
        (json!({"type":"query", "name":"key"}), "unsupported_auth"),
        (
            json!({"type":"header", "name":"bad\nname"}),
            "invalid_auth_header",
        ),
    ] {
        provider.auth = serde_json::from_value(auth).unwrap();
        let broker = http_broker(Arc::new(Grant::new(
            &provider,
            &connection,
            Some(&credentials),
        )))
        .unwrap();
        assert_eq!(
            broker
                .open("owner", request(&upstream.base))
                .await
                .unwrap_err()
                .code,
            expected
        );
    }
    provider.auth = serde_json::from_value(json!({"type":"bearer"})).unwrap();
    model::save_secret(&credentials, &connection.id, "bad\nsecret").unwrap();
    let broker = http_broker(Arc::new(Grant::new(
        &provider,
        &connection,
        Some(&credentials),
    )))
    .unwrap();
    assert_eq!(
        broker
            .open("owner", request(&upstream.base))
            .await
            .unwrap_err()
            .code,
        "invalid_auth"
    );
    upstream.assert_requests(1);
}

#[tokio::test]
async fn p08_handles_remain_broker_scoped_after_calls_and_provider_cancellation() {
    use ene_plugin::{Operation, ServiceRequest};
    use ene_plugin_runtime::instance::Services;
    let upstream = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &upstream.base);
    let open = json!({"type":"http_open", "request":{
        "method":"GET", "url":upstream.base, "headers":{}
    }})
    .to_string();
    let body = format!(
        "{} local.get $p call $service drop {} {}",
        guest_write(&open),
        emit(r#"{"type":"models","models":[]}"#),
        reply("{}")
    );
    let providers = providers(root.path(), Extension::Llm, &body, &upstream.base).await;
    let running = providers.running("openrouter", Extension::Llm).unwrap();
    let broker = http_broker(Arc::new(Grant::new(running.provider(), &connection, None))).unwrap();
    for _ in 0..2 {
        let services = BrokerServices::new(Arc::clone(&broker), "owner".into());
        running
            .invoke(
                Operation::ListModels {
                    endpoint: upstream.base.clone(),
                    options: json!({}),
                },
                services,
            )
            .await
            .unwrap();
    }
    running.cancel.cancel();
    let foreign = BrokerServices::new(Arc::clone(&broker), "foreign-provider".into());
    assert!(
        running
            .invoke(
                Operation::ListModels {
                    endpoint: upstream.base.clone(),
                    options: json!({})
                },
                foreign.clone()
            )
            .await
            .is_err()
    );
    foreign.check().await.unwrap();
    let old = foreign
        .handle(ServiceRequest::HttpRead {
            handle: "http-1".into(),
        })
        .await
        .unwrap();
    assert!(old["done"].as_bool().unwrap());
    assert_eq!(broker.read("http-1").unwrap_err().code, "unknown_handle");
    drop(foreign);
    drop(running);
    drop(providers);
    assert!(broker.read("http-2").unwrap().done);
    let separate = http_broker(Arc::new(ene_plugin_runtime::service::DenyHttp)).unwrap();
    assert_eq!(separate.read("http-2").unwrap_err().code, "unknown_handle");
    let weak = Arc::downgrade(&broker);
    drop(broker);
    assert!(weak.upgrade().is_none());
    upstream.assert_requests(2);
}
