mod lifecycle;
use super::*;
use ene_plugin::{CharacterEvent, CharacterTarget, HarnessInput, Service};
fn application(manager: &Manager) -> crate::application::HostApplication {
    crate::bootstrap::application_with_plugins(
        manager.store.clone(),
        "private-token".into(),
        Arc::new(manager.clone()),
    )
    .application
}
async fn invoke_plugin(manager: &Manager, input: PluginInvocation) -> Result<PluginResult> {
    application(manager).invoke_plugin(input).await
}
pub(crate) async fn manager() -> (Manager, tempfile::TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugins");
    ene_platform::storage::create_private_dir_all(&root).unwrap();
    let package = root.join("reference");
    ene_platform::storage::create_private_dir_all(&package).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/reference");
    ene_platform::storage::write_atomic(
        &package.join("manifest.json"),
        &std::fs::read(source.join("manifest.json")).unwrap(),
    )
    .unwrap();
    let wasm = std::env::var_os("ENE_TEST_REFERENCE_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            source.join("../../../target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm")
        });
    ene_platform::storage::write_atomic(
        &package.join("reference.wasm"),
        &std::fs::read(wasm)
            .expect("build ene-reference-wasm for wasm32-unknown-unknown before host tests"),
    )
    .unwrap();
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let mut manager = Manager::discover(store);
    manager.root = root;
    manager.entries.write().unwrap().clear();
    manager.scan().unwrap();
    (manager, temp, companion)
}
fn binding(
    manager: &Manager,
    companion: &str,
    id: &str,
    entrypoint: &str,
    services: Vec<Service>,
) -> PluginBinding {
    PluginBinding {
        id: id.into(),
        plugin_id: "reference".into(),
        entrypoint: entrypoint.into(),
        revision: manager.entries.read().unwrap()["reference"]
            .package
            .revision
            .clone(),
        companion_id: companion.into(),
        device_id: "device".into(),
        services: services.into_iter().map(name).collect(),
        configuration: document(json!({})).into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn embedded_character_state_permissions_updates_and_disable() {
    let (manager, _temp, companion) = manager().await;
    let revision = manager.entries.read().unwrap()["reference"]
        .package
        .revision
        .clone();
    assert_eq!(
        manager
            .list()
            .into_iter()
            .find(|p| p.id == "reference")
            .unwrap()
            .state,
        "disabled"
    );
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: true,
                revision: revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = binding(
        &manager,
        &companion,
        "behavior",
        "behavior",
        vec![Service::State],
    );
    manager.bind(b.clone()).await.unwrap();
    let invoke = || PluginInvocation {
        binding_id: b.id.clone(),
        operation: document(
            serde_json::to_value(Operation::Character {
                target: CharacterTarget {
                    device_id: "device".into(),
                    character_id: companion.clone(),
                },
                event: CharacterEvent::Activated,
            })
            .unwrap(),
        )
        .into(),
        ..Default::default()
    };
    for target in [
        CharacterTarget {
            device_id: "other".into(),
            character_id: companion.clone(),
        },
        CharacterTarget {
            device_id: "device".into(),
            character_id: "other".into(),
        },
    ] {
        let mut request = invoke();
        request.operation = document(
            serde_json::to_value(Operation::Character {
                target,
                event: CharacterEvent::Activated,
            })
            .unwrap(),
        )
        .into();
        assert_eq!(
            invoke_plugin(&manager, request).await.err().unwrap().kind,
            ErrorKind::PermissionRequired
        );
    }
    assert_eq!(
        serde_json::from_str::<Value>(
            invoke_plugin(&manager, invoke())
                .await
                .unwrap()
                .result
                .json
                .as_str()
        )
        .unwrap()["visits"],
        1
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            invoke_plugin(&manager, invoke())
                .await
                .unwrap()
                .result
                .json
                .as_str()
        )
        .unwrap()["visits"],
        2
    );
    let mut foreign = b.clone();
    foreign.id = "other".into();
    foreign.companion_id = "other-companion".into();
    manager
        .store
        .plugin_set("bindings".into(), foreign.id.clone(), json!(foreign))
        .await
        .unwrap();
    let services = LocalServices {
        manager: manager.clone(),
        mutations: Arc::new(application(&manager)),
        binding: foreign,
        cancel: CancellationToken::new(),
    };
    assert_eq!(
        services
            .handle(ServiceRequest::StateRead {
                key: "visits".into()
            })
            .await
            .unwrap(),
        Value::Null
    );
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: false,
                revision,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(invoke_plugin(&manager, invoke()).await.is_err());
    assert!(manager.sessions.lock().unwrap().is_empty());
}
#[tokio::test]
async fn changed_or_missing_code_can_be_disabled_and_stays_disabled() {
    for remove in [false, true] {
        let (manager, _temp, companion) = manager().await;
        let revision = manager.entries.read().unwrap()["reference"]
            .package
            .revision
            .clone();
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: true,
                    revision: revision.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let b = binding(
            &manager,
            &companion,
            "behavior",
            "behavior",
            vec![Service::State],
        );
        manager.bind(b.clone()).await.unwrap();
        invoke_plugin(&manager, PluginInvocation {
            binding_id: b.id,
            operation: document(json!({"type":"character","target":{"device_id":"device","character_id":companion},"event":{"type":"activated"}})).into(),
            ..Default::default()
        }).await.unwrap();
        let entry = manager.entries.read().unwrap()["reference"].clone();
        let file = entry.package.root.join("reference.wasm");
        let original = std::fs::read(&file).unwrap();
        if remove {
            std::fs::remove_file(&file).unwrap();
        } else {
            ene_platform::storage::write_atomic(&file, b"changed").unwrap();
        }
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: false,
                    revision,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(entry.cancellation.is_cancelled());
        assert!(manager.sessions.lock().unwrap().is_empty());
        assert!(manager.ready("reference", None).is_err());
        let saved = manager
            .store
            .plugin_get("enabled".into(), "reference".into())
            .await
            .unwrap();
        let saved: EnablePluginRequest = serde_json::from_value(saved["input"].clone()).unwrap();
        assert!(!saved.enabled);
        ene_platform::storage::write_atomic(&file, &original).unwrap();
        manager.restore().await.unwrap();
        assert!(manager.ready("reference", None).is_err());
    }
}

#[tokio::test]
async fn enable_changes_survive_waiter_cancellation_and_restore() {
    use std::time::Duration;
    for enabled in [false, true] {
        let (manager, _temp, _) = manager().await;
        let revision = manager.entries.read().unwrap()["reference"]
            .package
            .revision
            .clone();
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: !enabled,
                    revision: revision.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let lifetime = manager.entries.read().unwrap()["reference"]
            .cancellation
            .child_token();
        let worker = Arc::new(SessionState::new("reference".into(), lifetime.clone()));
        manager
            .sessions
            .lock()
            .unwrap()
            .insert("held".into(), worker.clone());
        let held = worker.worker.lock().await;
        let app = crate::bootstrap::application_with_plugins(
            manager.store.clone(),
            "private-token".into(),
            Arc::new(manager.clone()),
        )
        .application;
        let other = app.clone();
        let waiter = tokio::spawn(async move {
            other
                .enable_plugin(
                    "reference".into(),
                    EnablePluginRequest {
                        enabled,
                        revision,
                        ..Default::default()
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), lifetime.cancelled())
            .await
            .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(held);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let saved = manager
                    .store
                    .plugin_get("enabled".into(), "reference".into())
                    .await
                    .unwrap();
                let saved: EnablePluginRequest =
                    serde_json::from_value(saved["input"].clone()).unwrap();
                if saved.enabled == enabled {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the dispatched change must finish saving without its waiter");
        assert_eq!(manager.ready("reference", None).is_ok(), enabled);
        assert!(manager.sessions.lock().unwrap().is_empty());
        manager.restore().await.unwrap();
        assert_eq!(manager.ready("reference", None).is_ok(), enabled);
        app.stop().await.unwrap();
    }
}

#[tokio::test]
async fn stale_revisions_undeclared_grants_and_missing_targets_are_rejected() {
    let (manager, _temp, companion) = manager().await;
    assert!(
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: true,
                    revision: "old".into(),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    {
        let mut entry = manager.entries.write().unwrap();
        entry.get_mut("reference").unwrap().state = "ready".into();
    }
    let b = binding(
        &manager,
        &companion,
        "binding",
        "behavior",
        vec![Service::Http],
    );
    assert!(manager.bind(b).await.is_err());
    let b = binding(
        &manager,
        "foreign",
        "binding",
        "behavior",
        vec![Service::State],
    );
    assert!(manager.bind(b).await.is_err());
    let mut b = binding(
        &manager,
        &companion,
        "binding",
        "behavior",
        vec![Service::State],
    );
    b.revision = "old".into();
    assert!(manager.bind(b).await.is_err());
    let package = manager.entries.read().unwrap()["reference"].package.clone();
    crate::test_support::write(package.root.join("reference.wasm"), "return {}").unwrap();
    assert_eq!(
        manager
            .list()
            .into_iter()
            .find(|p| p.id == "reference")
            .unwrap()
            .state,
        "changed"
    );
    // Authorization uses the captured revision; executable verification happens at invocation boundaries.
    assert!(manager.ready("reference", None).is_ok());
    assert!(matches!(
        Instance::load(
            package,
            "behavior".into(),
            Target::Host,
            1,
            &CancellationToken::new()
        )
        .await,
        Err(ene_plugin_runtime::Error::Changed)
    ));
}

#[tokio::test]
async fn moved_packages_use_the_new_root_and_bad_packages_do_not_hide_good_ones() {
    let (manager, _temp, _) = manager().await;
    let old = manager.root.join("reference");
    let moved = manager.root.join("moved");
    std::fs::rename(&old, &moved).unwrap();
    manager.scan().unwrap();
    let entry = manager.entries.read().unwrap()["reference"].clone();
    assert_eq!(entry.package.root, moved);
    assert_eq!(entry.state, "disabled");

    let bad = manager.root.join("bad");
    crate::test_support::create_directory(&bad).unwrap();
    crate::test_support::write(
        bad.join("manifest.json"),
        r#"{"contract_version":3,"id":"bad","name":"Bad","version":"1","entrypoints":[]}"#,
    )
    .unwrap();
    manager.scan().unwrap();
    assert!(manager.entries.read().unwrap().contains_key("reference"));
    assert!(manager.discovery_error.read().unwrap().is_some());
    assert!(
        manager
            .list()
            .iter()
            .any(|plugin| plugin.id == "plugin-discovery" && plugin.state == "failed")
    );
}
#[tokio::test]
async fn harness_and_speech_share_the_wasm_contract() {
    let (manager, _temp, companion) = manager().await;
    let revision = manager.entries.read().unwrap()["reference"]
        .package
        .revision
        .clone();
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: true,
                revision,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .bind(binding(&manager, &companion, "task", "harness", vec![]))
        .await
        .unwrap();
    for input in [
        HarnessInput::Start {
            instruction: "test".into(),
        },
        HarnessInput::Result {
            operation_id: "model-1".into(),
            result: Value::Null,
        },
        HarnessInput::Result {
            operation_id: "tool-1".into(),
            result: Value::Null,
        },
        HarnessInput::Answer {
            question_id: "question-1".into(),
            answer: "done".into(),
        },
    ] {
        if matches!(input, HarnessInput::Answer { .. }) {
            let mut journal = manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap();
            journal["events"] = json!(vec![
                Event::Harness {
                    event: HarnessEvent::Progress {
                        text: "step".into()
                    }
                };
                MAX_EVENTS
            ]);
            manager
                .store
                .plugin_set("harness".into(), "task".into(), journal)
                .await
                .unwrap();
        }
        let result = invoke_plugin(
            &manager,
            PluginInvocation {
                binding_id: "task".into(),
                operation: document(serde_json::to_value(Operation::Harness { input }).unwrap())
                    .into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.events.len(), 1);
    }
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["report"], "done");
    assert_eq!(journal["events"].as_array().unwrap().len(), MAX_EVENTS);
    manager
        .bind(binding(&manager, &companion, "speech", "speech", vec![]))
        .await
        .unwrap();
    let result=invoke_plugin(&manager, PluginInvocation {binding_id:"speech".into(),operation:document(json!({"type":"speech","input":{"type":"synthesize","utterance_id":"u","text":"test","voice":"reference"}})).into(), ..Default::default() }).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result.events[0].json).unwrap()["chunk"]["sequence"],
        0
    );
    assert_eq!(
        serde_json::from_str::<Value>(&result.events[0].json).unwrap()["chunk"]["done"],
        true
    );
    let before = manager
        .sessions
        .lock()
        .unwrap()
        .get("speech")
        .cloned()
        .unwrap();
    let second=invoke_plugin(&manager, PluginInvocation {binding_id:"speech".into(),operation:document(json!({"type":"speech","input":{"type":"synthesize","utterance_id":"v","text":"again","voice":"reference"}})).into(), ..Default::default() }).await.unwrap();
    assert_eq!(second.generation, result.generation);
    let after = manager
        .sessions
        .lock()
        .unwrap()
        .get("speech")
        .cloned()
        .unwrap();
    assert!(Arc::ptr_eq(&before, &after));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn harness_commit_failure_is_terminal_and_allows_a_new_task() {
    let (manager, _temp, companion) = manager().await;
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    manager
        .bind(binding(&manager, &companion, "task", "harness", vec![]))
        .await
        .unwrap();
    let invoke = |input| {
        invoke_plugin(
            &manager,
            PluginInvocation {
                binding_id: "task".into(),
                operation: document(serde_json::to_value(Operation::Harness { input }).unwrap())
                    .into(),
                ..Default::default()
            },
        )
    };
    invoke(HarnessInput::Start {
        instruction: "test".into(),
    })
    .await
    .unwrap();
    let mut journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    journal["events"] = json!(vec![
        Event::Harness {
            event: HarnessEvent::Progress {
                text: "step".into()
            }
        };
        MAX_EVENTS
    ]);
    manager
        .store
        .plugin_set("harness".into(), "task".into(), journal)
        .await
        .unwrap();
    assert!(
        invoke(HarnessInput::Result {
            operation_id: "model-1".into(),
            result: Value::Null
        })
        .await
        .is_err()
    );
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "failed");
    assert_eq!(journal["report"], "commit_failed");
    assert_eq!(journal["events"].as_array().unwrap().len(), MAX_EVENTS);
    assert!(manager.sessions.lock().unwrap().is_empty());
    invoke(HarnessInput::Start {
        instruction: "retry".into(),
    })
    .await
    .unwrap();
    invoke(HarnessInput::Cancel).await.unwrap();
    manager.shutdown().await.unwrap();
}

fn kernel_bytes(value: &str) -> String {
    let mut code = format!("i64.const {} call $alloc local.set $p\n", value.len());
    for (index, byte) in value.bytes().enumerate() {
        code += &format!("local.get $p i64.const {index} i64.add i32.const {byte} call $store\n");
    }
    code
}

#[tokio::test]
async fn backpressured_provider_releases_slots_on_disable_cancel_and_trap() {
    use crate::{
        domain::OperationKind,
        llm::{ProviderBinding, test_support::InvocationFixture},
    };
    use ene_plugin_runtime::limits::MAX_INSTANCES;
    use futures_util::StreamExt;
    use std::time::Duration;

    for ending in ["disable", "cancel", "trap"] {
        let (manager, _temp, _) = manager().await;
        let saturated = Arc::new(tokio::sync::Notify::new());
        let reached = saturated.clone();
        let release = CancellationToken::new();
        let wait = release.clone();
        let router = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move || async move {
                reached.notify_one();
                wait.cancelled().await;
                ""
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let caps_text = r#"{"streaming_text":true,"model_list":false}"#;
        let caps = kernel_bytes(caps_text);
        let delta = kernel_bytes(r#"{"type":"delta","text":"x"}"#);
        let open = kernel_bytes(
            &json!({"type":"http_open","request":{
                "method":"POST","url":format!("{endpoint}/chat/completions"),"headers":{},"body_base64":null
            }})
            .to_string(),
        );
        // With no reader, 33 events fill both 16-item queues and the forwarder's send.
        let code = wat::parse_str(format!(
            r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (import "extism:host/user" "ene_emit" (func $emit (param i64)))
          (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
          (func (export "ene_initialize") (result i32) (local $p i64)
            {caps} local.get $p i64.const {} call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            (loop $again
              {delta} local.get $p call $emit
              local.get $i i32.const 1 i32.add local.tee $i i32.const 33 i32.lt_u br_if $again)
            {open} local.get $p call $service drop unreachable))"#,
            caps_text.len()
        ))
        .unwrap();
        let package = manager.root.join("backpressure");
        crate::test_support::create_directory(&package).unwrap();
        crate::test_support::write(package.join("main.wasm"), code).unwrap();
        crate::test_support::write(package.join("manifest.json"), json!({
            "contract_version":ene_plugin::CONTRACT_VERSION,"id":"backpressure","name":"Backpressure","version":"1",
            "entrypoints":[{"id":"llm","target":"host","file":"main.wasm","provides":["llm"],"uses":["http"],
            "provider":{"id":"backpressure","endpoint":endpoint,"auth":{"type":"none"},"destinations":[endpoint]}}]
        }).to_string()).unwrap();
        manager.scan().unwrap();
        manager.enable_provider("backpressure", true).await.unwrap();
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let fixture = InvocationFixture {
            operation: OperationKind::Conversation,
            id: "backpressure".into(),
            connection: crate::domain::Connection {
                id: "connection".into(),
                kind: "llm".into(),
                adapter: "backpressure".into(),
                endpoint,
                options: document(json!({})).into(),
                ..Default::default()
            },
            request: None,
            secret: None,
            authorization: Arc::new(Permit),
            cancel: receiver,
        };
        let provider = manager.provider("backpressure").unwrap();
        let binding = ProviderBinding::resolve(provider.clone(), &fixture.connection).unwrap();
        let mut stream = provider.generate(fixture.build(binding)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), saturated.notified())
            .await
            .unwrap();
        let hold = CancellationToken::new();
        for _ in 1..MAX_INSTANCES {
            let hold = hold.clone();
            manager
                .supervisor
                .spawn(async move { hold.cancelled().await })
                .unwrap();
        }
        match ending {
            "disable" => manager
                .enable_provider("backpressure", false)
                .await
                .unwrap(),
            "cancel" => {
                stop.send_replace(true);
            }
            "trap" => release.cancel(),
            _ => unreachable!(),
        }
        let freed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let hold = hold.clone();
                if manager
                    .supervisor
                    .spawn(async move { hold.cancelled().await })
                    .is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        hold.cancel();
        release.cancel();
        let mut failed = false;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(crate::llm::Event::Delta(_)) => {}
                    Err(_) => failed = true,
                    _ => panic!("a stopped invocation must not report completion"),
                }
            }
        })
        .await
        .unwrap();
        manager.shutdown().await.unwrap();
        server.abort();
        assert!(
            freed.is_ok(),
            "{ending} retained the supervisor slot until output was read"
        );
        assert!(
            failed,
            "{ending} must remain an error even when its final error could not be queued"
        );
    }
}

pub(super) struct Permit;
impl crate::llm::Authorization for Permit {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn provider_reuses_one_worker_between_generations() {
    use crate::llm::{Generation, ProviderBinding, test_support::InvocationFixture};
    use futures_util::StreamExt;

    let (manager, _temp, _) = manager().await;
    let caps_text = r#"{"streaming_text":true,"model_list":false}"#;
    let caps = kernel_bytes(caps_text);
    let finished = kernel_bytes(r#"{"type":"finished","reason":"stop"}"#);
    let empty = kernel_bytes("{}");
    let code = wat::parse_str(format!(
        r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (import "extism:host/user" "ene_emit" (func $emit (param i64)))
          (func (export "ene_initialize") (result i32) (local $p i64)
            {caps} local.get $p i64.const {} call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64)
            {finished} local.get $p call $emit
            {empty} local.get $p i64.const 2 call $output i32.const 0))"#,
        caps_text.len()
    ))
    .unwrap();
    let package = manager.root.join("reuse");
    crate::test_support::create_directory(&package).unwrap();
    crate::test_support::write(package.join("main.wasm"), code).unwrap();
    crate::test_support::write(
        package.join("manifest.json"),
        json!({
            "contract_version":ene_plugin::CONTRACT_VERSION,
            "id":"reuse",
            "name":"Reuse",
            "version":"1",
            "entrypoints":[{
                "id":"llm","target":"host","file":"main.wasm","provides":["llm"],"uses":[],
                "provider":{
                    "id":"reuse","endpoint":"https://example.com/api",
                    "auth":{"type":"none"},"destinations":["https://example.com"]
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    manager.scan().unwrap();
    manager.enable_provider("reuse", true).await.unwrap();
    let provider = manager.provider("reuse").unwrap();
    let connection = crate::domain::Connection {
        id: "connection".into(),
        kind: "llm".into(),
        adapter: "reuse".into(),
        endpoint: "https://example.com/api".into(),
        options: document(json!({})).into(),
        ..Default::default()
    };
    let make = |cancel| InvocationFixture {
        operation: crate::domain::OperationKind::Conversation,
        id: "generation".into(),
        connection: connection.clone(),
        request: Some(Generation::for_test(
            connection.endpoint.clone(),
            json!({}),
            "model".into(),
            String::new(),
            vec![],
            8,
        )),
        secret: None,
        authorization: Arc::new(Permit),
        cancel,
    };
    let (_first_stop, first_cancel) = tokio::sync::watch::channel(false);
    let binding = ProviderBinding::resolve(provider.clone(), &connection).unwrap();
    let mut first = provider
        .generate(make(first_cancel).build(binding))
        .await
        .unwrap();
    while let Some(event) = first.next().await {
        event.unwrap();
    }
    let before = manager
        .provider_sessions
        .lock()
        .unwrap()
        .get("reuse:llm")
        .cloned()
        .unwrap();
    let (_second_stop, second_cancel) = tokio::sync::watch::channel(false);
    let binding = ProviderBinding::resolve(provider.clone(), &connection).unwrap();
    let mut second = provider
        .generate(make(second_cancel).build(binding))
        .await
        .unwrap();
    while let Some(event) = second.next().await {
        event.unwrap();
    }
    let after = manager
        .provider_sessions
        .lock()
        .unwrap()
        .get("reuse:llm")
        .cloned()
        .unwrap();
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(manager.provider_sessions.lock().unwrap().len(), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn wasm_provider_streams_through_real_broker_and_cancels_pending_reads() {
    use crate::domain::OperationKind;
    use crate::llm::{
        Event as LlmEvent, Generation, ProviderBinding, test_support::InvocationFixture,
    };
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for outcome in ["complete", "cancel", "trap", "abort"] {
        let aborted = outcome == "abort";
        let (manager, _temp, _) = manager().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let second = Arc::new(tokio::sync::Notify::new());
        let release = second.clone();
        let opened = Arc::new(tokio::sync::Notify::new());
        let request_seen = opened.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = socket.read_u8().await.unwrap();
                request.push(byte);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            request_seen.notify_one();
            if !aborted {
                socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n5\r\nfirst\r\n").await.unwrap();
            }
            release.notified().await;
            let _ = socket.write_all(b"6\r\nsecond\r\n0\r\n\r\n").await;
        });
        let caps = kernel_bytes(r#"{"streaming_text":true,"model_list":false}"#);
        let open=kernel_bytes(&json!({"type":"http_open","request":{"method":"POST","url":format!("{endpoint}/chat/completions"),"headers":{},"body_base64":null}}).to_string());
        let mut first = format!(
            "{} local.get $p call $emit",
            kernel_bytes(r#"{"type":"delta","text":"first"}"#)
        );
        let mut finished = format!(
            "{} local.get $p call $emit",
            kernel_bytes(r#"{"type":"finished","reason":"stop"}"#)
        );
        // Receiving this HTTP request proves the guest emitted completion before blocking.
        let before_open = if aborted {
            format!(
                "{} {}",
                std::mem::take(&mut first),
                std::mem::take(&mut finished)
            )
        } else {
            String::new()
        };
        let after_finished = if outcome == "trap" { "unreachable" } else { "" };
        let prefix = r#"{"type":"http_read","handle":""#;
        let read = kernel_bytes(&format!("{prefix}{}\"}}", "0".repeat(36)));
        let mut copy = String::new();
        for i in 0..36 {
            copy += &format!(
                "local.get $p i64.const {} i64.add local.get $handle i64.const {i} i64.add call $load call $store\n",
                prefix.len() + i
            );
        }
        let code=wat::parse_str(format!(r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "load_u8" (func $load (param i64) (result i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
          (import "extism:host/user" "ene_emit" (func $emit (param i64)))
          (func (export "ene_initialize") (result i32) (local $p i64) {caps} local.get $p i64.const 42 call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64) (local $handle i64)
            {before_open} {open} local.get $p call $service local.set $handle
            (loop $scan
                local.get $handle call $load i32.const 104 i32.ne
                local.get $handle i64.const 1 i64.add call $load i32.const 97 i32.ne i32.or
                local.get $handle i64.const 2 i64.add call $load i32.const 110 i32.ne i32.or
                if local.get $handle i64.const 1 i64.add local.set $handle br $scan end)
            local.get $handle i64.const 9 i64.add local.set $handle
            {first}
            {read} {copy} local.get $p call $service drop {finished} {after_finished}
            i64.const 2 call $alloc local.set $p local.get $p i32.const 123 call $store
            local.get $p i64.const 1 i64.add i32.const 125 call $store
            local.get $p i64.const 2 call $output i32.const 0))"#)).unwrap();
        let package = manager.root.join("wasm");
        crate::test_support::create_directory(&package).unwrap();
        crate::test_support::write(package.join("main.wasm"), code).unwrap();
        crate::test_support::write(package.join("manifest.json"),json!({"contract_version":ene_plugin::CONTRACT_VERSION,"id":"wasm-fixture","name":"Wasm fixture","version":"1","entrypoints":[{"id":"llm","target":"host","file":"main.wasm","provides":["llm"],"uses":["http"],"provider":{"id":"wasm-fixture","endpoint":endpoint,"auth":{"type":"none"},"destinations":[endpoint]}}]}).to_string()).unwrap();
        manager.scan().unwrap();
        let revision = manager.entries.read().unwrap()["wasm-fixture"]
            .package
            .revision
            .clone();
        manager
            .enable(
                "wasm-fixture".into(),
                EnablePluginRequest {
                    enabled: true,
                    revision: revision.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let fixture = InvocationFixture {
            operation: OperationKind::Conversation,
            id: "generate".into(),
            connection: crate::domain::Connection {
                id: "connection".into(),
                kind: "llm".into(),
                adapter: "wasm-fixture".into(),
                endpoint: endpoint.clone(),
                options: document(json!({})).into(),
                credential_state: "missing".into(),
                capabilities: Default::default(),
                last_check: Default::default(),
                ..Default::default()
            },
            request: Some(Generation::for_test(
                endpoint,
                json!({}),
                "fixture".into(),
                "".into(),
                vec![],
                64,
            )),
            secret: None,
            authorization: Arc::new(Permit),
            cancel: receiver,
        };
        let provider = manager.provider("wasm-fixture").unwrap();
        let binding = ProviderBinding::resolve(provider.clone(), &fixture.connection).unwrap();
        let input = fixture.build(binding);
        let mut stream = provider.generate(input).await.unwrap();
        assert!(
            matches!(tokio::time::timeout(ene_plugin_runtime::limits::INITIALIZE_TIMEOUT + std::time::Duration::from_secs(10),stream.next()).await.unwrap().unwrap().unwrap(),LlmEvent::Delta(text) if text=="first")
        );
        if aborted {
            tokio::time::timeout(std::time::Duration::from_secs(4), opened.notified())
                .await
                .unwrap();
            manager.supervisor.shutdown().await;
            assert!(
                stream.next().await.unwrap().is_err(),
                "an aborted forwarder must never publish completion"
            );
            server.abort();
        } else if outcome == "cancel" {
            stop.send_replace(true);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(4), stream.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            server.abort();
        } else {
            second.notify_one();
            server.await.unwrap();
            let next = tokio::time::timeout(std::time::Duration::from_secs(4), stream.next())
                .await
                .unwrap()
                .unwrap();
            if outcome == "trap" {
                assert!(
                    next.is_err(),
                    "a failed guest must never publish completion"
                );
            } else {
                assert!(matches!(next.unwrap(), LlmEvent::Finished(reason) if reason == "stop"));
            }
        }
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn failed_starts_release_single_use_and_persistent_slots() {
    let (manager, _temp, companion) = manager().await;
    ene_platform::storage::write_atomic(
        &manager.root.join("reference/reference.wasm"),
        b"invalid wasm",
    )
    .unwrap();
    manager.scan().unwrap();
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();

    manager
        .bind(binding(&manager, &companion, "speech", "speech", vec![]))
        .await
        .unwrap();
    manager
        .bind(binding(
            &manager,
            &companion,
            "behavior",
            "behavior",
            vec![Service::State],
        ))
        .await
        .unwrap();
    manager
        .bind(binding(&manager, &companion, "task", "harness", vec![]))
        .await
        .unwrap();
    for _ in 0..32 {
        for input in [
            PluginInvocation {
                binding_id: "speech".into(),
                operation: document(json!({"type":"speech","input":{"type":"synthesize","utterance_id":"u","text":"test","voice":"reference"}})).into(),
             ..Default::default() },
            PluginInvocation {
                binding_id: "task".into(),
                operation: document(serde_json::to_value(Operation::Harness {
                    input: HarnessInput::Start {
                        instruction: "test".into(),
                    },
                })
                .unwrap()).into(),
             ..Default::default() },
            PluginInvocation {
                binding_id: "behavior".into(),
                operation: document(serde_json::to_value(Operation::Character {
                target: CharacterTarget { device_id: "device".into(), character_id: companion.clone() },
                    event: CharacterEvent::Activated,
                })
                .unwrap()).into(),
             ..Default::default() },
        ] {
            assert!(invoke_plugin(&manager, input).await.is_err());
            assert!(manager.sessions.lock().unwrap().is_empty());
            assert_eq!(manager.invocation_slots.available_permits(), 16);
        }
    }
}

#[tokio::test]
async fn harness_cancel_signals_before_the_worker_lock_and_persists_terminal_state() {
    cancel_waiter(false).await;
    cancel_waiter(true).await;
}
async fn cancel_waiter(abort_waiter: bool) {
    let (manager, _temp, companion) = manager().await;
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    let binding = binding(&manager, &companion, "task", "harness", vec![]);
    manager.bind(binding.clone()).await.unwrap();
    let journal = manager
        .harness_begin(
            &binding,
            &HarnessInput::Start {
                instruction: "test".into(),
            },
        )
        .await
        .unwrap();
    manager
        .harness_commit(&binding.id, journal, &{
            let mut events = vec![Event::Harness {
                event: HarnessEvent::Question {
                    question_id: "q".into(),
                    text: "?".into(),
                },
            }];
            events.extend(std::iter::repeat_n(
                Event::Harness {
                    event: HarnessEvent::Progress {
                        text: "step".into(),
                    },
                },
                MAX_EVENTS - 1,
            ));
            events
        })
        .await
        .unwrap();
    let lifetime = CancellationToken::new();
    let worker = Arc::new(SessionState::new("reference".into(), lifetime.clone()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert("task".into(), worker.clone());
    let active = lifetime.child_token();
    let _invocation = ActiveInvocation::new(worker.clone(), active.clone());
    let held = worker.worker.lock().await;
    let app = crate::bootstrap::application_with_plugins(
        manager.store.clone(),
        "private-token".into(),
        Arc::new(manager.clone()),
    )
    .application;
    let other = app.clone();
    let cancel = tokio::spawn(async move {
        other
            .invoke_plugin(PluginInvocation {
                binding_id: "task".into(),
                operation: document(
                    serde_json::to_value(Operation::Harness {
                        input: HarnessInput::Cancel,
                    })
                    .unwrap(),
                )
                .into(),
                ..Default::default()
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), active.cancelled())
        .await
        .unwrap();
    assert!(!lifetime.is_cancelled());
    assert!(!cancel.is_finished());
    if abort_waiter {
        cancel.abort();
        assert!(cancel.await.unwrap_err().is_cancelled());
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let journal = manager
                    .store
                    .plugin_get("harness".into(), "task".into())
                    .await
                    .unwrap();
                if journal["status"] == "cancelled" && manager.sessions.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    } else {
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(1), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap()["status"],
        "cancelled"
    );
    assert!(manager.sessions.lock().unwrap().is_empty());
    assert!(!lifetime.child_token().is_cancelled());
    app.stop().await.unwrap();
}

#[tokio::test]
async fn old_invocation_drop_preserves_replacement_cancel_registration() {
    let session = Arc::new(SessionState::new(
        "reference".into(),
        CancellationToken::new(),
    ));
    let old_token = CancellationToken::new();
    let new_token = CancellationToken::new();
    let old = ActiveInvocation::new(session.clone(), old_token.clone());
    let new = ActiveInvocation::new(session.clone(), new_token.clone());
    drop(old);
    assert!(old_token.is_cancelled());
    let active = session.active.lock().unwrap();
    let (id, token) = active.as_ref().unwrap();
    assert_eq!(*id, new.id);
    token.cancel();
    assert!(new_token.is_cancelled());
    drop(active);
    drop(new);
    assert!(session.active.lock().unwrap().is_none());
}

#[tokio::test]
async fn rebind_cancels_before_waiting_and_old_session_drop_preserves_replacement() {
    let (manager, _temp, companion) = manager().await;
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    let binding = binding(
        &manager,
        &companion,
        "behavior",
        "behavior",
        vec![Service::State],
    );
    manager.bind(binding.clone()).await.unwrap();
    let old = Arc::new(SessionState::new(
        "reference".into(),
        CancellationToken::new(),
    ));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert(binding.id.clone(), old.clone());
    let held = Session {
        guard: old.worker.lock().await,
        retain: false,
        registration: Some((manager.clone(), binding.id.clone(), old.clone())),
        _permit: None,
    };
    let cancel = old.lifetime.child_token();
    let _invocation = ActiveInvocation::new(old.clone(), cancel.clone());
    let other = manager.clone();
    let input = binding.clone();
    let rebind = tokio::spawn(async move { other.bind(input).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), cancel.cancelled())
        .await
        .unwrap();
    assert!(!rebind.is_finished());
    let replacement = Arc::new(SessionState::new(
        "reference".into(),
        CancellationToken::new(),
    ));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert(binding.id.clone(), replacement.clone());
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(1), rebind)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(
        &manager.sessions.lock().unwrap()[&binding.id],
        &replacement
    ));
    assert!(!replacement.lifetime.is_cancelled());
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_during_worker_startup_is_cancelled_and_confirmation_is_idempotent() {
    for stop_host in [false, true] {
        let (manager, _temp, companion) = manager().await;
        manager
            .entries
            .write()
            .unwrap()
            .get_mut("reference")
            .unwrap()
            .state = "ready".into();
        let wasm = wat::parse_str(
            r#"(module
        (func (export "ene_initialize") (result i32) (loop $forever br $forever) i32.const 0)
        (func (export "ene_invoke") (result i32) i32.const 0))"#,
        )
        .unwrap();
        ene_platform::storage::write_atomic(&manager.root.join("reference/reference.wasm"), &wasm)
            .unwrap();
        manager.scan().unwrap();
        manager
            .entries
            .write()
            .unwrap()
            .get_mut("reference")
            .unwrap()
            .state = "ready".into();
        let binding = binding(&manager, &companion, "task", "harness", vec![]);
        manager.bind(binding.clone()).await.unwrap();
        let app = crate::bootstrap::application_with_plugins(
            manager.store.clone(),
            "private-token".into(),
            Arc::new(manager.clone()),
        )
        .application;
        let other = app.clone();
        let start = tokio::spawn(async move {
            other
                .invoke_plugin(PluginInvocation {
                    binding_id: "task".into(),
                    operation: document(
                        serde_json::to_value(Operation::Harness {
                            input: HarnessInput::Start {
                                instruction: "test".into(),
                            },
                        })
                        .unwrap(),
                    )
                    .into(),
                    ..Default::default()
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if manager
                    .store
                    .plugin_get("harness".into(), "task".into())
                    .await
                    .unwrap()["status"]
                    == "running"
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if stop_host {
            app.begin_shutdown();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(3), start)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            tokio::time::timeout(std::time::Duration::from_secs(3), app.stop())
                .await
                .unwrap()
                .unwrap();
            let journal = manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap();
            assert_eq!(journal["status"], "failed");
            assert_eq!(journal["report"], "plugin_stopped");
            assert!(manager.sessions.lock().unwrap().is_empty());
            continue;
        }
        let cancel = || PluginInvocation {
            binding_id: "task".into(),
            operation: document(
                serde_json::to_value(Operation::Harness {
                    input: HarnessInput::Cancel,
                })
                .unwrap(),
            )
            .into(),
            ..Default::default()
        };
        let confirmed = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            invoke_plugin(&manager, cancel()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(start.await.unwrap().is_err());
        let journal = manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        assert_eq!(journal["status"], "cancelled");
        assert_eq!(journal["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(confirmed.result.json.as_str()).unwrap()["task_id"],
            journal["task_id"]
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                invoke_plugin(&manager, cancel())
                    .await
                    .unwrap()
                    .result
                    .json
                    .as_str()
            )
            .unwrap()["task_id"],
            journal["task_id"]
        );
        manager
            .harness_begin(
                &binding,
                &HarnessInput::Start {
                    instruction: "again".into(),
                },
            )
            .await
            .unwrap();
        assert!(manager.sessions.lock().unwrap().is_empty());
        assert_eq!(manager.invocation_slots.available_permits(), 16);
        app.stop().await.unwrap();
    }
}

#[tokio::test]
async fn connect_management_invokes_the_wasm_harness_and_enforces_host_authentication() {
    use crate::api::test_client::ApiClient;
    use connectrpc::ErrorCode;
    let (manager, temp, companion) = manager().await;
    let store = manager.store.clone();
    let manager = Arc::new(manager);
    let revision = manager.entries.read().unwrap()["reference"]
        .package
        .revision
        .clone();
    let app =
        crate::bootstrap::application_with_plugins(store, "private-token".into(), manager.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/rpc", listener.local_addr().unwrap());
    let router = crate::api::router(app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = ApiClient::new(&endpoint, "private-token").unwrap();
    let unauthorized = ApiClient::new(&endpoint, "wrong").unwrap();
    assert!(matches!(
        unauthorized
            .plugins
            .list_with_options(
                ene_rpc::Empty::default(),
                crate::api::test_client::call_options(5)
            )
            .await
            .map(|response| response.into_owned().items)
            ,
        Err(error) if error.code == ErrorCode::Unauthenticated
    ));
    assert!(
        client
            .plugins
            .list_with_options(
                ene_rpc::Empty::default(),
                crate::api::test_client::call_options(5)
            )
            .await
            .unwrap()
            .into_owned()
            .items
            .iter()
            .any(|p| p.id == "reference" && p.state == "disabled")
    );
    client
        .plugins
        .enable_with_options(
            EnablePluginRequest {
                id: "reference".into(),
                enabled: true,
                revision,
                ..Default::default()
            },
            crate::api::test_client::call_options(60),
        )
        .await
        .unwrap();
    let bound = binding(&manager, &companion, "connect-task", "harness", vec![]);
    client
        .plugins
        .bind_with_options(bound.clone(), crate::api::test_client::call_options(10))
        .await
        .unwrap();
    let result = client
        .plugins
        .invoke_with_options(
            PluginInvocation {
                binding_id: bound.id,
                operation: document(
                    json!({"type":"harness","input":{"type":"start","instruction":"日本語🙂"}}),
                )
                .into(),
                ..Default::default()
            },
            crate::api::test_client::call_options(190),
        )
        .await
        .unwrap()
        .into_owned();
    assert!(
        result
            .events
            .iter()
            .any(|e| serde_json::from_str::<Value>(&e.json).unwrap()["type"] == "harness")
    );
    assert_eq!(
        client
            .plugins
            .list_bindings_with_options(
                ene_rpc::Empty::default(),
                crate::api::test_client::call_options(5)
            )
            .await
            .unwrap()
            .into_owned()
            .items
            .len(),
        1
    );
    app.application.stop().await.unwrap();
    server.abort();
    drop(temp);
}

#[tokio::test]
async fn event_count_and_byte_overflow_stop_the_invocation_promptly() {
    for (count, text) in [(300, "x".into()), (100, "x".repeat(16384))] {
        let (manager, _temp, companion) = manager().await;
        let root = manager.entries.read().unwrap()["reference"]
            .package
            .root
            .clone();
        let empty = kernel_bytes("{}");
        let event = kernel_bytes(&json!({"type":"character","target":{"device_id":"device","character_id":companion},"action":{"type":"speak","text":text}}).to_string());
        let code = wat::parse_str(format!(
            r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (import "extism:host/user" "ene_emit" (func $emit (param i64)))
          (func (export "ene_initialize") (result i32) (local $p i64)
            {empty} local.get $p i64.const 2 call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            {event}
            (loop $next local.get $p call $emit
              local.get $i i32.const 1 i32.add local.tee $i i32.const {count} i32.lt_u br_if $next)
            {empty} local.get $p i64.const 2 call $output i32.const 0))"#
        ))
        .unwrap();
        ene_platform::storage::write_atomic(&root.join("reference.wasm"), &code).unwrap();
        manager.scan().unwrap();
        let revision = manager.entries.read().unwrap()["reference"]
            .package
            .revision
            .clone();
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: true,
                    revision,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let b = binding(&manager, &companion, "overflow", "behavior", vec![]);
        manager.bind(b.clone()).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), invoke_plugin(&manager, PluginInvocation {
            binding_id: b.id,
            operation: document(json!({"type":"character","target":{"device_id":"device","character_id":companion},"event":{"type":"activated"}})).into(),
            ..Default::default()
        })).await;
        manager.entries.read().unwrap()["reference"]
            .cancellation
            .cancel();
        assert_eq!(
            result
                .expect("event overflow must stop immediately")
                .unwrap_err()
                .kind,
            ErrorKind::InvalidRequest
        );
        assert!(manager.sessions.lock().unwrap().is_empty());
    }
}
