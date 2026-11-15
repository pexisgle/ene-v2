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
    let path = root.join(PLUGINS_DIR).join("openrouter");
    let capabilities = r#"{"streaming_text":true,"model_list":true,"tool_calls":true}"#;
    let wasm = format!(
        r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
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
                    "auth": {"type": "none"}, "destinations": [destination],
                    "options": {"fixture_option": "preserved"}
                }
            }]
        })
        .to_string(),
    );
    let providers = Providers::load(root);
    providers.start("openrouter", extension).await.unwrap();
    providers
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
