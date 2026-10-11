use super::*;
use ene_plugin::{CharacterEvent, HarnessInput, Service};
pub(crate) async fn manager() -> (Manager, tempfile::TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugins");
    ene_platform::storage::create_private_dir_all(&root).unwrap();
    let package = root.join("reference");
    ene_platform::storage::create_private_dir_all(&package).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/reference");
    for file in [
        "manifest.json",
        "character.luau",
        "harness.luau",
        "speech.luau",
    ] {
        ene_platform::storage::write_atomic(
            &package.join(file),
            &std::fs::read(source.join(file)).unwrap(),
        )
        .unwrap();
    }
    let store = Store::open(temp.path().join("host.sqlite3")).await.unwrap();
    let companion = store.snapshot().await.unwrap().companions[0].id.clone();
    let mut manager = Manager::discover(store);
    manager.root = root;
    manager.cache = temp.path().join("cache");
    manager.runner = std::env::var_os("ENE_PLUGIN_RUNNER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| ene_platform::sibling_executable("ene-plugin-runner"));
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
        services: services.into_iter().map(name).collect(),
        configuration: document(json!({})).into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn isolated_character_state_permissions_updates_and_disable() {
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
                event: CharacterEvent::Activated,
            })
            .unwrap(),
        )
        .into(),
        ..Default::default()
    };
    assert_eq!(
        serde_json::from_str::<Value>(manager.invoke(invoke()).await.unwrap().result.json.as_str())
            .unwrap()["visits"],
        1
    );
    assert_eq!(
        serde_json::from_str::<Value>(manager.invoke(invoke()).await.unwrap().result.json.as_str())
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
    assert!(manager.invoke(invoke()).await.is_err());
    assert_eq!(std::fs::read_dir(&manager.cache).unwrap().count(), 0);
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
    crate::test_support::write(package.root.join("character.luau"), "return {}").unwrap();
    assert_eq!(
        manager
            .list()
            .into_iter()
            .find(|p| p.id == "reference")
            .unwrap()
            .state,
        "changed"
    );
    assert!(manager.ready("reference", None).is_err());
}
#[tokio::test]
async fn harness_and_speech_share_the_isolated_contract() {
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
        let result = manager
            .invoke(PluginInvocation {
                binding_id: "task".into(),
                operation: document(serde_json::to_value(Operation::Harness { input }).unwrap())
                    .into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.events.len(), 1);
    }
    manager
        .bind(binding(&manager, &companion, "speech", "speech", vec![]))
        .await
        .unwrap();
    let result=manager.invoke(PluginInvocation {binding_id:"speech".into(),operation:document(json!({"type":"speech","input":{"type":"synthesize","utterance_id":"u","text":"test","voice":"reference"}})).into(), ..Default::default() }).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result.events[0].json).unwrap()["chunk"]["sequence"],
        0
    );
    assert_eq!(
        serde_json::from_str::<Value>(&result.events[0].json).unwrap()["chunk"]["done"],
        true
    );
    manager.shutdown().await;
}

fn kernel_bytes(value: &str) -> String {
    let mut code = format!("i64.const {} call $alloc local.set $p\n", value.len());
    for (index, byte) in value.bytes().enumerate() {
        code += &format!("local.get $p i64.const {index} i64.add i32.const {byte} call $store\n");
    }
    code
}
struct Permit;
impl ene_core::ports::llm::Authorization for Permit {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn wasm_provider_streams_through_real_broker_and_cancels_pending_reads() {
    use ene_core::domain::OperationKind;
    use ene_core::ports::llm::{
        Event as LlmEvent, Generation, ProviderBinding, test_support::InvocationFixture,
    };
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for cancelled in [false, true] {
        let (manager, _temp, _) = manager().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let second = Arc::new(tokio::sync::Notify::new());
        let release = second.clone();
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
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n5\r\nfirst\r\n").await.unwrap();
            release.notified().await;
            let _ = socket.write_all(b"6\r\nsecond\r\n0\r\n\r\n").await;
        });
        let caps = kernel_bytes(r#"{"streaming_text":true,"model_list":false}"#);
        let open=kernel_bytes(&json!({"type":"http_open","request":{"method":"GET","url":endpoint,"headers":{},"body_base64":null}}).to_string());
        let first = kernel_bytes(r#"{"type":"delta","text":"first"}"#);
        let finished = kernel_bytes(r#"{"type":"finished","reason":"stop"}"#);
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
            {open} local.get $p call $service local.set $handle
            (loop $scan
                local.get $handle call $load i32.const 104 i32.ne
                local.get $handle i64.const 1 i64.add call $load i32.const 97 i32.ne i32.or
                local.get $handle i64.const 2 i64.add call $load i32.const 110 i32.ne i32.or
                if local.get $handle i64.const 1 i64.add local.set $handle br $scan end)
            local.get $handle i64.const 9 i64.add local.set $handle
            {first} local.get $p call $emit
            {read} {copy} local.get $p call $service drop {finished} local.get $p call $emit
            i64.const 2 call $alloc local.set $p local.get $p i32.const 123 call $store
            local.get $p i64.const 1 i64.add i32.const 125 call $store
            local.get $p i64.const 2 call $output i32.const 0))"#)).unwrap();
        let package = manager.root.join("wasm");
        crate::test_support::create_directory(&package).unwrap();
        crate::test_support::write(package.join("main.wasm"), code).unwrap();
        crate::test_support::write(package.join("manifest.json"),json!({"contract_version":1,"id":"wasm-fixture","name":"Wasm fixture","version":"1","code_files":["main.wasm"],"entrypoints":[{"id":"llm","target":"host","runtime":"extism","file":"main.wasm","provides":["llm"],"uses":["http"],"provider":{"id":"wasm-fixture","endpoint":endpoint,"auth":{"type":"none"},"destinations":[endpoint]}}]}).to_string()).unwrap();
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
            connection: ene_core::domain::Connection {
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
        if cancelled {
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
            assert!(
                matches!(tokio::time::timeout(std::time::Duration::from_secs(4),stream.next()).await.unwrap().unwrap().unwrap(),LlmEvent::Finished(reason) if reason=="stop")
            );
        }
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn failed_starts_release_single_use_and_persistent_slots() {
    let (mut manager, _temp, companion) = manager().await;
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    manager.runner = PathBuf::from("/missing/ene-plugin-runner");
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
                    event: CharacterEvent::Activated,
                })
                .unwrap()).into(),
             ..Default::default() },
        ] {
            assert!(manager.invoke(input).await.is_err());
            assert!(manager.sessions.lock().unwrap().is_empty());
            assert!(manager.binding_tokens.lock().unwrap().is_empty());
            assert!(manager.invocation_tokens.lock().unwrap().is_empty());
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
        .store
        .plugin_set("harness".into(), "task".into(), json!(journal))
        .await
        .unwrap();
    let worker = Arc::new(Mutex::new(None));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert("task".into(), worker.clone());
    let lifetime = CancellationToken::new();
    manager
        .binding_tokens
        .lock()
        .unwrap()
        .insert("task".into(), ("reference".into(), lifetime.clone()));
    let active = lifetime.child_token();
    manager
        .invocation_tokens
        .lock()
        .unwrap()
        .insert("task".into(), (uuid::Uuid::new_v4(), active.clone()));
    let held = worker.lock().await;
    let other = manager.clone();
    let cancel = tokio::spawn(async move {
        other
            .invoke(PluginInvocation {
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
}

#[tokio::test]
async fn old_invocation_drop_preserves_replacement_cancel_registration() {
    let (manager, _temp, _) = manager().await;
    let old_token = CancellationToken::new();
    let new_token = CancellationToken::new();
    let old = ActiveInvocation::new(manager.clone(), "task".into(), old_token.clone());
    let new = ActiveInvocation::new(manager.clone(), "task".into(), new_token.clone());
    drop(old);
    assert!(old_token.is_cancelled());
    let tokens = manager.invocation_tokens.lock().unwrap();
    let (id, token) = &tokens["task"];
    assert_eq!(*id, new.id);
    token.cancel();
    assert!(new_token.is_cancelled());
    drop(tokens);
    drop(new);
    assert!(manager.invocation_tokens.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_during_worker_startup_is_cancelled_and_confirmation_is_idempotent() {
    use std::os::unix::fs::PermissionsExt;
    let (mut manager, temp, companion) = manager().await;
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    // A real isolated child that consumes Initialize but never sends Ready.
    let runner = temp.path().join("waiting-runner");
    crate::test_support::write(&runner, "#!/bin/sh\nread request\nread shutdown\n").unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    manager.runner = runner;
    let binding = binding(&manager, &companion, "task", "harness", vec![]);
    manager.bind(binding.clone()).await.unwrap();
    let other = manager.clone();
    let start = tokio::spawn(async move {
        other
            .invoke(PluginInvocation {
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
                && manager.cache.exists()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
    let confirmed =
        tokio::time::timeout(std::time::Duration::from_secs(3), manager.invoke(cancel()))
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
        serde_json::from_str::<Value>(manager.invoke(cancel()).await.unwrap().result.json.as_str())
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
}

#[tokio::test]
async fn connect_management_invokes_the_isolated_harness_and_enforces_host_authentication() {
    use ene_api_client::{ApiClient, Error};
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
    let client = ApiClient::new(&ene_api_client::RuntimeConnection {
        endpoint: endpoint.clone(),
        token: "private-token".into(),
    })
    .unwrap();
    let unauthorized = ApiClient::new(&ene_api_client::RuntimeConnection {
        endpoint,
        token: "wrong".into(),
    })
    .unwrap();
    assert!(matches!(
        unauthorized.plugins().await,
        Err(Error::Unauthorized)
    ));
    assert!(
        client
            .plugins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == "reference" && p.state == "disabled")
    );
    client
        .enable_plugin(
            "reference",
            &EnablePluginRequest {
                enabled: true,
                revision,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let bound = binding(&manager, &companion, "connect-task", "harness", vec![]);
    client.bind_plugin(&bound).await.unwrap();
    let result = client
        .invoke_plugin(&PluginInvocation {
            binding_id: bound.id,
            operation: document(
                json!({"type":"harness","input":{"type":"start","instruction":"日本語🙂"}}),
            )
            .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        result
            .events
            .iter()
            .any(|e| serde_json::from_str::<Value>(&e.json).unwrap()["type"] == "harness")
    );
    assert_eq!(client.plugin_bindings().await.unwrap().len(), 1);
    app.application.stop().await.unwrap();
    server.abort();
    drop(temp);
}
