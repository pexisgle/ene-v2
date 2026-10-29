use super::*;
use ene_plugin::{CharacterEvent, CharacterTarget, HarnessInput, Service};
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
            manager.invoke(request).await.err().unwrap().kind,
            ErrorKind::PermissionRequired
        );
    }
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
    assert!(manager.sessions.lock().unwrap().is_empty());
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
    assert!(manager.ready("reference", None).is_err());
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
impl crate::llm::Authorization for Permit {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn wasm_provider_streams_through_real_broker_and_cancels_pending_reads() {
    use crate::domain::OperationKind;
    use crate::llm::{
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
            assert!(manager.invoke(input).await.is_err());
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
        .store
        .plugin_set("harness".into(), "task".into(), json!(journal))
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
