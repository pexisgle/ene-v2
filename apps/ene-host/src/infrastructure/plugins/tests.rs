mod lifecycle;
use super::invoke::LocalServices;
use super::sessions::{ActiveInvocation, Session, SessionState};
use super::*;
use crate::domain::harness::{MAX_TRANSCRIPT, Session as HarnessSession, Status as HarnessStatus};
use ene_plugin::{
    CharacterEvent, CharacterTarget, ContentChunk, GenerationRequest, HarnessEvent, HarnessInput,
    Operation, RequestPermissionOutcome, SelectedPermissionOutcome, Service, ServiceRequest,
    SessionUpdate, ToolCall, ToolCallUpdate, ToolCallUpdateFields, text_block,
};
#[cfg(unix)]
use ene_plugin_runtime::limits::MAX_EVENTS;
#[cfg(unix)]
use ene_rpc::TerminalPolicy;
use futures_util::future::BoxFuture;
use std::sync::atomic::Ordering;
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
fn harness_invocation(input: HarnessInput) -> PluginInvocation {
    PluginInvocation {
        binding_id: "task".into(),
        operation: document(serde_json::to_value(Operation::Harness { input }).unwrap()).into(),
        ..Default::default()
    }
}
fn task_start(prompt: &str) -> HarnessInput {
    HarnessInput::Start {
        prompt: vec![text_block(prompt)],
    }
}
fn allow() -> HarnessInput {
    HarnessInput::Respond {
        request_id: "permission-1".into(),
        outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
    }
}
pub(crate) async fn manager() -> (Manager, tempfile::TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugins");
    crate::test_support::create_directory(&root).unwrap();
    let package = root.join("reference");
    crate::test_support::create_directory(&package).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/reference");
    crate::test_support::write(
        package.join("manifest.json"),
        std::fs::read(source.join("manifest.json")).unwrap(),
    )
    .unwrap();
    let wasm = std::env::var_os("ENE_TEST_REFERENCE_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            source.join("../../../target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm")
        });
    crate::test_support::write(
        package.join("reference.wasm"),
        std::fs::read(wasm)
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
fn binding_to(
    manager: &Manager,
    id: &str,
    plugin_id: &str,
    entrypoint: &str,
    companion: &str,
) -> PluginBinding {
    PluginBinding {
        id: id.into(),
        plugin_id: plugin_id.into(),
        entrypoint: entrypoint.into(),
        revision: manager.entries.read().unwrap()[plugin_id]
            .package
            .revision
            .clone(),
        companion_id: companion.into(),
        device_id: "device".into(),
        configuration: document(json!({})).into(),
        ..Default::default()
    }
}
fn character_guest(marker: &str) -> Vec<u8> {
    let caps = kernel_bytes("{}");
    let result = format!(r#"{{"plugin":"{marker}"}}"#);
    let value = kernel_bytes(&result);
    wat::parse_str(format!(
        r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (func (export "ene_initialize") (result i32) (local $p i64)
            {caps} local.get $p i64.const 2 call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64)
            {value} local.get $p i64.const {} call $output i32.const 0))"#,
        result.len()
    ))
    .unwrap()
}
#[tokio::test]
async fn a_session_from_another_binding_is_never_reused() {
    let (manager, _temp, companion) = manager().await;
    for (id, marker) in [("p", "P"), ("q", "Q")] {
        let package = manager.root.join(id);
        crate::test_support::create_directory(&package).unwrap();
        crate::test_support::write(package.join("main.wasm"), character_guest(marker)).unwrap();
        crate::test_support::write(
            package.join("manifest.json"),
            json!({
                "contract_version":ene_plugin::CONTRACT_VERSION,
                "id":id,"name":id,"version":"1",
                "entrypoints":[{
                    "id":"behavior","target":"host","file":"main.wasm",
                    "provides":["character"],"uses":[]
                }]
            })
            .to_string(),
        )
        .unwrap();
    }
    manager.scan().unwrap();
    for id in ["p", "q"] {
        let revision = manager.entries.read().unwrap()[id].package.revision.clone();
        manager
            .enable(
                id.into(),
                EnablePluginRequest {
                    enabled: true,
                    revision,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let mut current = binding_to(&manager, "x", "p", "behavior", &companion);
    manager.bind(current.clone()).await.unwrap();
    let call = || PluginInvocation {
        binding_id: "x".into(),
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
    assert_eq!(
        invoke_plugin(&manager, call()).await.unwrap().result.json,
        r#"{"plugin":"P"}"#
    );
    // A rebind that lands after this session was created must not reuse its guest.
    current.plugin_id = "q".into();
    current.revision = manager.entries.read().unwrap()["q"]
        .package
        .revision
        .clone();
    manager
        .store
        .plugin_set(
            "bindings".into(),
            "x".into(),
            serde_json::to_value(&current).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        invoke_plugin(&manager, call()).await.unwrap_err().kind,
        ErrorKind::StateConflict
    );
    assert_eq!(
        invoke_plugin(&manager, call()).await.unwrap().result.json,
        r#"{"plugin":"Q"}"#
    );
    manager.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_stopping_session_does_not_remove_its_replacement() {
    let (manager, _temp, companion) = manager().await;
    let binding = binding_to(&manager, "x", "reference", "behavior", &companion);
    let session = Arc::new(SessionState::new(&binding, CancellationToken::new()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert("x".into(), session.clone());
    // Park the stop inside harness_stop, after it checked that the entry is its own.
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let store = manager.store.clone();
    let (mark, gate) = (entered.clone(), release.clone());
    let blocked = tokio::spawn(async move {
        store
            .run(move |_| {
                mark.store(true, Ordering::SeqCst);
                let (lock, condvar) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = condvar.wait(open).unwrap();
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let stop = {
        let manager = manager.clone();
        let session = session.clone();
        tokio::spawn(async move { manager.stop_session("x", &session).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while session.worker.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replacement = Arc::new(SessionState::new(&binding, CancellationToken::new()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert("x".into(), replacement.clone());
    {
        let (lock, condvar) = &*release;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
    stop.await.unwrap().unwrap();
    blocked.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(
        &manager.sessions.lock().unwrap()["x"],
        &replacement
    ));
    manager.shutdown().await.unwrap();
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
async fn a_finished_enable_cannot_persist_over_a_later_disable() {
    let (manager, _temp, _) = manager().await;
    let revision = manager.entries.read().unwrap()["reference"]
        .package
        .revision
        .clone();
    // Hold the store so the enable stops between its generation check and its save.
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let store = manager.store.clone();
    let (mark, gate) = (entered.clone(), release.clone());
    let blocked = tokio::spawn(async move {
        store
            .run(move |_| {
                mark.store(true, Ordering::SeqCst);
                let (lock, condvar) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = condvar.wait(open).unwrap();
                }
                Ok(())
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let enable = {
        let manager = manager.clone();
        let revision = revision.clone();
        tokio::spawn(async move {
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
        })
    };
    let state = || manager.entries.read().unwrap()["reference"].state.clone();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while state() != "ready" {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let disable = {
        let manager = manager.clone();
        let revision = revision.clone();
        tokio::spawn(async move {
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
        })
    };
    // The disable has entered the enable/disable path before we release the
    // old enable's blocked save. This is a control point, not a time guess.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while manager.enable_entries.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the later disable did not start");
    assert_eq!(state(), "ready", "a later disable must wait for the commit");
    {
        let (lock, condvar) = &*release;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
    enable.await.unwrap().unwrap();
    disable.await.unwrap().unwrap();
    blocked.await.unwrap().unwrap();
    assert_eq!(state(), "disabled");
    let saved = manager
        .store
        .plugin_get("enabled".into(), "reference".into())
        .await
        .unwrap();
    let saved: EnablePluginRequest = serde_json::from_value(saved["input"].clone()).unwrap();
    assert!(!saved.enabled);
    manager.restore().await.unwrap();
    assert!(manager.ready("reference", None).is_err());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_accepts_identical_code_rewritten_on_disk() {
    let (manager, _temp, _) = manager().await;
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
    let file = manager.root.join("reference/reference.wasm");
    let code = std::fs::read(&file).unwrap();
    crate::test_support::write(&file, &code).unwrap();
    let state = |manager: &Manager| {
        manager
            .list()
            .into_iter()
            .find(|plugin| plugin.id == "reference")
            .unwrap()
            .state
    };
    assert_eq!(state(&manager), "changed");
    manager.reload().await.unwrap();
    assert_eq!(state(&manager), "ready");
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_keeps_an_enable_that_finishes_during_the_scan() {
    let (manager, _temp, _) = manager().await;
    let revision = manager.entries.read().unwrap()["reference"]
        .package
        .revision
        .clone();
    // The scan reads the files before the enable completes.
    let (packages, errors) = manager.load_packages().unwrap();
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
    manager.apply_scan(packages, errors).unwrap();
    assert!(manager.ready("reference", None).is_ok());
    assert_eq!(
        manager
            .list()
            .into_iter()
            .find(|plugin| plugin.id == "reference")
            .unwrap()
            .state,
        "ready"
    );
    manager.shutdown().await.unwrap();
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
            crate::test_support::write(&file, b"changed").unwrap();
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
        crate::test_support::write(&file, &original).unwrap();
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
        let binding = PluginBinding {
            plugin_id: "reference".into(),
            entrypoint: "behavior".into(),
            ..Default::default()
        };
        let worker = Arc::new(SessionState::new(&binding, lifetime.clone()));
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
        Instance::load(package, "behavior".into(), 1, &CancellationToken::new()).await,
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
async fn harness_turn_uses_the_wasm_contract() {
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
    let started = invoke_plugin(&manager, harness_invocation(task_start("test")))
        .await
        .unwrap();
    assert_eq!(started.events.len(), 4);
    let journal: HarnessSession = serde_json::from_value(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(journal.status, HarnessStatus::WaitingForInput);
    assert!(journal.pending.is_some());
    let finished = invoke_plugin(&manager, harness_invocation(allow()))
        .await
        .unwrap();
    assert!(!finished.events.is_empty());
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["stop_reason"], "end_turn");
    assert!(
        journal["report"]
            .as_str()
            .unwrap()
            .contains("参照タスクを完了しました。")
    );
    manager.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_long_harness_turn_keeps_the_newest_events() {
    let (manager, _temp, companion) = manager().await;
    let init = kernel_bytes("{}");
    let chunk = kernel_bytes(
        &serde_json::to_string(&ene_plugin::Event::Harness {
            event: HarnessEvent::Update {
                update: SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block("x"))),
            },
        })
        .unwrap(),
    );
    let stop = kernel_bytes(
        &serde_json::to_string(&ene_plugin::Event::Harness {
            event: HarnessEvent::Stop {
                stop_reason: ene_plugin::StopReason::EndTurn,
            },
        })
        .unwrap(),
    );
    let wasm = wat::parse_str(format!(
        r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64)
          {init} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
          (loop $again
            {chunk} local.get $p call $emit
            local.get $i i32.const 1 i32.add local.tee $i i32.const 300 i32.lt_u br_if $again)
          {stop} local.get $p call $emit
          {init} local.get $p i64.const 2 call $output i32.const 0))"#
    ))
    .unwrap();
    crate::test_support::write(manager.root.join("reference/reference.wasm"), &wasm).unwrap();
    manager.scan().unwrap();
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    let binding = binding(&manager, &companion, "task", "harness", vec![]);
    manager.bind(binding).await.unwrap();
    let result = invoke_plugin(&manager, harness_invocation(task_start("test")))
        .await
        .unwrap();
    assert_eq!(result.events.len(), MAX_EVENTS);
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["report"], "x".repeat(300));
    let events = journal["events"].as_array().unwrap();
    assert_eq!(events.len(), MAX_TRANSCRIPT);
    assert!(matches!(
        serde_json::from_value::<HarnessEvent>(events.last().unwrap().clone()).unwrap(),
        HarnessEvent::Stop { .. }
    ));
    assert_eq!(
        serde_json::from_str::<Value>(result.events.last().unwrap().json.as_str()).unwrap()["type"],
        "harness"
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn harness_transcript_is_bounded_and_a_terminal_turn_allows_a_new_task() {
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
    invoke_plugin(&manager, harness_invocation(task_start("test")))
        .await
        .unwrap();
    let mut journal: HarnessSession = serde_json::from_value(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap(),
    )
    .unwrap();
    let extra = HarnessEvent::Update {
        update: SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block("step"))),
    };
    for _ in 0..MAX_TRANSCRIPT + 5 {
        manager
            .harness_apply("task", &mut journal, &extra)
            .await
            .unwrap();
    }
    assert_eq!(journal.events.len(), MAX_TRANSCRIPT);
    invoke_plugin(&manager, harness_invocation(allow()))
        .await
        .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    let events = journal["events"].as_array().unwrap();
    assert_eq!(events.len(), MAX_TRANSCRIPT);
    assert!(matches!(
        serde_json::from_value::<HarnessEvent>(events.last().unwrap().clone()).unwrap(),
        HarnessEvent::Stop { .. }
    ));
    assert!(manager.sessions.lock().unwrap().is_empty());
    invoke_plugin(&manager, harness_invocation(task_start("retry")))
        .await
        .unwrap();
    invoke_plugin(&manager, harness_invocation(HarnessInput::Cancel))
        .await
        .unwrap();
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
async fn concurrent_provider_generations_do_not_share_a_worker() {
    use crate::llm::{Generation, ProviderBinding, test_support::InvocationFixture};
    use futures_util::StreamExt;
    use std::time::Duration;

    let (manager, _temp, _) = manager().await;
    let caps_text = r#"{"streaming_text":true,"model_list":false}"#;
    let caps = kernel_bytes(caps_text);
    let delta = kernel_bytes(r#"{"type":"delta","text":"x"}"#);
    let finished = kernel_bytes(r#"{"type":"finished","reason":"stop"}"#);
    let empty = kernel_bytes("{}");
    // With no reader, 33 events fill both 16-item queues and block the guest.
    let code = wat::parse_str(format!(
        r#"(module
          (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
          (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
          (import "extism:host/env" "output_set" (func $output (param i64 i64)))
          (import "extism:host/user" "ene_emit" (func $emit (param i64)))
          (func (export "ene_initialize") (result i32) (local $p i64)
            {caps} local.get $p i64.const {} call $output i32.const 0)
          (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            (loop $again
              {delta} local.get $p call $emit
              local.get $i i32.const 1 i32.add local.tee $i i32.const 33 i32.lt_u br_if $again)
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
    // One event proves the guest owns an instance and is now blocked on backpressure.
    first.next().await.unwrap().unwrap();
    let (_second_stop, second_cancel) = tokio::sync::watch::channel(false);
    let binding = ProviderBinding::resolve(provider.clone(), &connection).unwrap();
    let mut second = provider
        .generate(make(second_cancel).build(binding))
        .await
        .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut finished = false;
        while let Some(event) = second.next().await {
            finished = matches!(event.unwrap(), crate::llm::Event::Finished(_));
        }
        finished
    })
    .await
    .expect("a second generation must not wait for a blocked instance");
    assert!(completed);
    drop(first);
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
async fn failed_starts_release_instance_slots() {
    let (manager, _temp, companion) = manager().await;
    crate::test_support::write(
        manager.root.join("reference/reference.wasm"),
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
                binding_id: "task".into(),
                operation: document(
                    serde_json::to_value(Operation::Harness {
                        input: task_start("test"),
                    })
                    .unwrap(),
                )
                .into(),
                ..Default::default()
            },
            PluginInvocation {
                binding_id: "behavior".into(),
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
            },
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
    let mut journal = HarnessSession::start("task-session");
    let call = HarnessEvent::Update {
        update: SessionUpdate::ToolCall(ToolCall::new("call-1", "step")),
    };
    manager
        .harness_apply(&binding.id, &mut journal, &call)
        .await
        .unwrap();
    let permission = HarnessEvent::RequestPermission {
        request_id: "q".into(),
        tool_call: ToolCallUpdate::new("call-1", ToolCallUpdateFields::new()),
        options: vec![ene_plugin::PermissionOption::new(
            "allow",
            "Allow",
            ene_plugin::PermissionOptionKind::AllowOnce,
        )],
    };
    manager
        .harness_apply(&binding.id, &mut journal, &permission)
        .await
        .unwrap();
    let lifetime = CancellationToken::new();
    let worker = Arc::new(SessionState::new(&binding, lifetime.clone()));
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
async fn a_pending_cancel_blocks_the_next_harness_start() {
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
    let session = Arc::new(SessionState::new(&binding, CancellationToken::new()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert("task".into(), session.clone());
    let active = session.lifetime.child_token();
    let _invocation = ActiveInvocation::new(session.clone(), active.clone());
    let held = session.worker.lock().await;
    let app = application(&manager);
    let cancel = {
        let app = app.clone();
        tokio::spawn(async move {
            app.invoke_plugin(PluginInvocation {
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
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), active.cancelled())
        .await
        .unwrap();
    assert!(!cancel.is_finished());
    // The old task finishes and its registration goes away while the cancel waits.
    manager.sessions.lock().unwrap().remove("task");
    let start = {
        let app = app.clone();
        tokio::spawn(async move {
            app.invoke_plugin(PluginInvocation {
                binding_id: "task".into(),
                operation: document(
                    serde_json::to_value(Operation::Harness {
                        input: task_start("next"),
                    })
                    .unwrap(),
                )
                .into(),
                ..Default::default()
            })
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !start.is_finished(),
        "a pending cancel must block the next start"
    );
    drop(held);
    assert!(cancel.await.unwrap().is_err());
    start.await.unwrap().unwrap();
    manager.shutdown().await.unwrap();
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
    let old = Arc::new(SessionState::new(&binding, CancellationToken::new()));
    manager
        .sessions
        .lock()
        .unwrap()
        .insert(binding.id.clone(), old.clone());
    let held = Session {
        guard: old.worker.lock().await,
        retain: false,
        registration: (manager.clone(), binding.id.clone(), old.clone()),
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
    let replacement = Arc::new(SessionState::new(&binding, CancellationToken::new()));
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
        crate::test_support::write(manager.root.join("reference/reference.wasm"), &wasm).unwrap();
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
                            input: task_start("test"),
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
            assert_eq!(journal["failure"], "plugin_stopped");
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
            journal["session_id"]
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
            journal["session_id"]
        );
        manager
            .harness_begin(&binding, &task_start("again"))
            .await
            .unwrap();
        assert!(manager.sessions.lock().unwrap().is_empty());
        assert_eq!(manager.invocation_slots.available_permits(), 16);
        app.stop().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_interrupts_a_running_harness_turn() {
    let (manager, _temp, companion) = manager().await;
    let init = kernel_bytes("{}");
    let wasm = wat::parse_str(format!(
        r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (func (export "ene_initialize") (result i32) (local $p i64)
          {init} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (loop $forever br $forever) i32.const 0))"#
    ))
    .unwrap();
    crate::test_support::write(manager.root.join("reference/reference.wasm"), &wasm).unwrap();
    manager.scan().unwrap();
    manager
        .entries
        .write()
        .unwrap()
        .get_mut("reference")
        .unwrap()
        .state = "ready".into();
    let binding = binding(&manager, &companion, "task", "harness", vec![]);
    manager.bind(binding).await.unwrap();
    let app = application(&manager);
    let other = app.clone();
    let start = tokio::spawn(async move {
        other
            .invoke_plugin(harness_invocation(task_start("test")))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let journal = manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap();
            if journal["status"] == "running" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the running turn must be recorded before cancellation");
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.invoke_plugin(harness_invocation(HarnessInput::Cancel)),
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
    assert_eq!(journal["stop_reason"], "cancelled");
    assert!(journal["pending"].is_null());
    assert!(manager.sessions.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(cancelled.result.json.as_str()).unwrap()["status"],
        "cancelled"
    );
    assert_eq!(manager.invocation_slots.available_permits(), 16);
    app.stop().await.unwrap();
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
    let mut events = client
        .host
        .subscribe(ene_rpc::Empty::default())
        .await
        .unwrap();
    let result = client
        .plugins
        .invoke_with_options(
            PluginInvocation {
                binding_id: bound.id.clone(),
                operation: document(
                    json!({"type":"harness","input":{"type":"start","prompt":[{"type":"text","text":"日本語🙂"}]}}),
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
    let result_json: Value = serde_json::from_str(&result.result.json).unwrap();
    let state = client
        .plugins
        .get_harness_with_options(
            ene_rpc::Id {
                id: bound.id.clone(),
                ..Default::default()
            },
            crate::api::test_client::call_options(5),
        )
        .await
        .unwrap()
        .into_owned();
    let state: Value = serde_json::from_str(&state.state.json).unwrap();
    assert_eq!(state["session_id"], result_json["task_id"]);
    assert_eq!(state["status"], "waiting_for_input");
    assert_eq!(state["pending"]["request_id"], "permission-1");
    assert!(matches!(
        unauthorized
            .plugins
            .get_harness_with_options(
                ene_rpc::Id {
                    id: bound.id.clone(),
                    ..Default::default()
                },
                crate::api::test_client::call_options(5)
            )
            .await,
        Err(error) if error.code == ErrorCode::Unauthenticated
    ));
    assert_eq!(
        client
            .plugins
            .get_harness_with_options(
                ene_rpc::Id {
                    id: "missing".into(),
                    ..Default::default()
                },
                crate::api::test_client::call_options(5)
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let mut task = None;
    for _ in 0..8 {
        let Some(message) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            events.message::<ene_rpc::HostEvent>(),
        )
        .await
        .unwrap()
        .unwrap() else {
            break;
        };
        let message = message.to_owned_message();
        if let Some(ene_rpc::host_event::Event::Notification(notification)) = message.event
            && let Some(ene_rpc::notification::Event::HarnessTask(event)) = notification.event
        {
            task = Some(event);
            break;
        }
    }
    let task = task.expect("harness task notification");
    assert_eq!(task.binding_id, bound.id);
    assert_eq!(task.status, "waiting_for_input");
    assert_eq!(task.task_id, result_json["task_id"].as_str().unwrap());
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
        crate::test_support::write(root.join("reference.wasm"), &code).unwrap();
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

struct FixedInference(Arc<std::sync::Mutex<Option<crate::ports::inference::InferenceRequest>>>);
impl crate::ports::inference::TaskInference for FixedInference {
    fn complete<'a>(
        &'a self,
        request: crate::ports::inference::InferenceRequest,
        _: CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(request);
            Ok("fixture".into())
        })
    }
}

struct NoMutations;
impl crate::ports::plugin::Mutations for NoMutations {
    fn submit(
        &self,
        _: futures_util::future::BoxFuture<'static, Result<()>>,
    ) -> futures_util::future::BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn model_request() -> GenerationRequest {
    GenerationRequest {
        endpoint: "https://guest.invalid".into(),
        options: json!({"guest": true}),
        model: "guest-model".into(),
        preamble: "system".into(),
        messages: vec![ene_plugin::provider::ChatMessage {
            role: "user".into(),
            text: "hi".into(),
        }],
        max_tokens: 512,
    }
}

#[tokio::test]
async fn the_models_service_forwards_a_host_owned_request() {
    let (manager, _temp, companion) = manager().await;
    let granted = binding(
        &manager,
        &companion,
        "task",
        "harness",
        vec![Service::Models],
    );
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: true,
                revision: granted.revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager.bind(granted.clone()).await.unwrap();
    let denied = binding(&manager, &companion, "task-denied", "harness", vec![]);
    manager.bind(denied.clone()).await.unwrap();
    let seen = Arc::new(std::sync::Mutex::new(None));
    manager.attach_inference(Arc::new(FixedInference(seen.clone())));
    let services = LocalServices {
        manager: manager.clone(),
        binding: granted.clone(),
        cancel: CancellationToken::new(),
        mutations: Arc::new(NoMutations),
    };
    let value = services
        .handle(ServiceRequest::Models {
            request: model_request(),
        })
        .await
        .unwrap();
    assert_eq!(value["text"], "fixture");
    let seen = seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.companion_id, companion);
    assert_eq!(seen.preamble, "system");
    assert_eq!(seen.messages, vec![(true, "hi".into())]);
    assert_eq!(seen.max_tokens, 512);

    let denied = LocalServices {
        manager: manager.clone(),
        binding: denied,
        cancel: CancellationToken::new(),
        mutations: Arc::new(NoMutations),
    };
    assert_eq!(
        denied
            .handle(ServiceRequest::Models {
                request: model_request(),
            })
            .await
            .unwrap_err()
            .code,
        "permission_required"
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_tools_service_reads_and_writes_inside_the_workspace() {
    let (manager, _temp, companion) = manager().await;
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let root_text = canonical.to_str().unwrap().to_owned();
    let mut granted = binding(
        &manager,
        &companion,
        "task",
        "harness",
        vec![Service::Tools],
    );
    granted.workspace_root = Some(root_text.clone());
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: true,
                revision: granted.revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager.bind(granted.clone()).await.unwrap();
    let stored = manager
        .bindings()
        .await
        .unwrap()
        .into_iter()
        .find(|binding| binding.id == granted.id)
        .unwrap();
    assert_eq!(stored.workspace_root.as_deref(), Some(root_text.as_str()));
    let denied = binding(&manager, &companion, "task-denied", "harness", vec![]);
    manager.bind(denied.clone()).await.unwrap();
    let no_root = binding(
        &manager,
        &companion,
        "task-no-root",
        "harness",
        vec![Service::Tools],
    );
    manager.bind(no_root.clone()).await.unwrap();

    let services = LocalServices {
        manager: manager.clone(),
        binding: granted.clone(),
        cancel: CancellationToken::new(),
        mutations: Arc::new(NoMutations),
    };
    assert_eq!(
        services
            .handle(ServiceRequest::Tool {
                name: "fs.write_text_file".into(),
                arguments: json!({"path":"note.txt","content":"hello"}),
            })
            .await
            .unwrap()["ok"],
        true
    );
    assert_eq!(
        services
            .handle(ServiceRequest::Tool {
                name: "fs.read_text_file".into(),
                arguments: json!({"path":"note.txt"}),
            })
            .await
            .unwrap()["content"],
        "hello"
    );
    assert_eq!(
        services
            .handle(ServiceRequest::Tool {
                name: "fs.read_text_file".into(),
                arguments: json!({"path":"../escape"}),
            })
            .await
            .unwrap_err()
            .code,
        "permission_required"
    );
    assert_eq!(
        services
            .handle(ServiceRequest::Tool {
                name: "shell".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err()
            .code,
        "unsupported_tool"
    );
    for unbound in [no_root, denied] {
        let services = LocalServices {
            manager: manager.clone(),
            binding: unbound,
            cancel: CancellationToken::new(),
            mutations: Arc::new(NoMutations),
        };
        assert_eq!(
            services
                .handle(ServiceRequest::Tool {
                    name: "fs.read_text_file".into(),
                    arguments: json!({"path":"note.txt"}),
                })
                .await
                .unwrap_err()
                .code,
            "permission_required"
        );
    }
    let mut bad = binding(
        &manager,
        &companion,
        "bad-root",
        "harness",
        vec![Service::Tools],
    );
    bad.workspace_root = Some("relative/path".into());
    assert_eq!(
        manager.bind(bad).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    let mut root_binding = binding(
        &manager,
        &companion,
        "root-dir",
        "harness",
        vec![Service::Tools],
    );
    root_binding.workspace_root = Some(std::path::MAIN_SEPARATOR.to_string());
    assert_eq!(
        manager.bind(root_binding).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    manager.shutdown().await.unwrap();
}

#[cfg(unix)]
const ACP_AGENT: &str = r#"
import json, sys
session = "s1"
prompt_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = msg["id"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "started"}, "messageId": "m1"}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "go"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif msg.get("id") == 900:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(msg["result"]["outcome"])}, "messageId": "m2"}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "cancelled"}})
"#;

#[cfg(unix)]
#[tokio::test]
async fn an_acp_agent_drives_a_harness_turn_and_permission() {
    let (manager, _temp, companion) = manager().await;
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("agent.py");
    std::fs::write(&script, ACP_AGENT).unwrap();
    let package = manager.root.join("acp-test");
    crate::test_support::create_directory(&package).unwrap();
    crate::test_support::write(
        package.join("manifest.json"),
        json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "acp-test",
            "name": "ACP test",
            "version": "1",
            "entrypoints": [{
                "id": "main",
                "target": "acp",
                "provides": ["harness"],
                "command": ["python3", script.to_str().unwrap()]
            }]
        })
        .to_string(),
    )
    .unwrap();
    manager.scan().unwrap();
    let revision = manager.entries.read().unwrap()["acp-test"]
        .package
        .revision
        .clone();
    manager
        .enable(
            "acp-test".into(),
            EnablePluginRequest {
                enabled: true,
                revision: revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let binding = PluginBinding {
        id: "acp-task".into(),
        plugin_id: "acp-test".into(),
        entrypoint: "main".into(),
        revision,
        companion_id: companion.clone(),
        workspace_root: Some(root.to_str().unwrap().into()),
        configuration: document(json!({})).into(),
        ..Default::default()
    };
    manager.bind(binding.clone()).await.unwrap();
    let invoke = |input: HarnessInput| PluginInvocation {
        binding_id: "acp-task".into(),
        operation: document(serde_json::to_value(Operation::Harness { input }).unwrap()).into(),
        ..Default::default()
    };
    invoke_plugin(
        &manager,
        invoke(HarnessInput::Start {
            prompt: vec![text_block("go")],
        }),
    )
    .await
    .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "acp-task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "waiting_for_input");
    let request_id = journal["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!request_id.is_empty());
    assert!(journal["report"].as_str().unwrap().contains("started"));
    invoke_plugin(
        &manager,
        invoke(HarnessInput::Respond {
            request_id,
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
        }),
    )
    .await
    .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "acp-task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["stop_reason"], "end_turn");
    assert!(journal["report"].as_str().unwrap().contains("allow"));
    assert!(manager.acp.lock().unwrap().is_empty());

    invoke_plugin(
        &manager,
        invoke(HarnessInput::Start {
            prompt: vec![text_block("again")],
        }),
    )
    .await
    .unwrap();
    invoke_plugin(&manager, invoke(HarnessInput::Cancel))
        .await
        .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "acp-task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "cancelled");
    assert!(manager.acp.lock().unwrap().is_empty());

    let mut without_root = binding.clone();
    without_root.id = "acp-without-root".into();
    without_root.workspace_root = None;
    assert_eq!(
        manager.bind(without_root).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    let mut with_services = binding;
    with_services.id = "acp-with-services".into();
    with_services.services = vec!["models".into()];
    assert_eq!(
        manager.bind(with_services).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    manager.shutdown().await.unwrap();
}

#[cfg(unix)]
const ACP_TERMINAL_AGENT: &str = r#"
import json, sys
session = "s1"
prompt_id = None
terminal_id = None
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        prompt_id = mid
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "started"}, "messageId": "m1"}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": "t1", "title": "run echo"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    elif mid == 900:
        send({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": session, "command": "echo", "args": ["hello"], "outputByteLimit": 64}})
    elif mid == 901:
        terminal_id = msg["result"]["terminalId"]
        send({"jsonrpc": "2.0", "id": 902, "method": "terminal/wait_for_exit", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 902:
        send({"jsonrpc": "2.0", "id": 903, "method": "terminal/output", "params": {"sessionId": session, "terminalId": terminal_id}})
    elif mid == 903:
        output = msg["result"]["output"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": output}, "messageId": "m2"}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn"}})
"#;

#[cfg(unix)]
#[tokio::test]
async fn an_acp_agent_runs_a_permission_gated_terminal_command() {
    let (manager, _temp, companion) = manager().await;
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("agent.py");
    std::fs::write(&script, ACP_TERMINAL_AGENT).unwrap();
    let package = manager.root.join("acp-terminal");
    crate::test_support::create_directory(&package).unwrap();
    crate::test_support::write(
        package.join("manifest.json"),
        json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "acp-terminal",
            "name": "ACP terminal",
            "version": "1",
            "entrypoints": [{
                "id": "main",
                "target": "acp",
                "provides": ["harness"],
                "command": ["python3", script.to_str().unwrap()]
            }]
        })
        .to_string(),
    )
    .unwrap();
    manager.scan().unwrap();
    let revision = manager.entries.read().unwrap()["acp-terminal"]
        .package
        .revision
        .clone();
    manager
        .enable(
            "acp-terminal".into(),
            EnablePluginRequest {
                enabled: true,
                revision: revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let binding = PluginBinding {
        id: "terminal-task".into(),
        plugin_id: "acp-terminal".into(),
        entrypoint: "main".into(),
        revision,
        companion_id: companion.clone(),
        workspace_root: Some(root.to_str().unwrap().into()),
        terminal: Some(TerminalPolicy {
            enabled: true,
            allowed_commands: vec!["echo".into()],
            max_seconds: 5,
            max_output_bytes: 1024,
            ..Default::default()
        })
        .into(),
        configuration: document(json!({})).into(),
        ..Default::default()
    };
    manager.bind(binding.clone()).await.unwrap();
    let invoke = |input: HarnessInput| PluginInvocation {
        binding_id: "terminal-task".into(),
        operation: document(serde_json::to_value(Operation::Harness { input }).unwrap()).into(),
        ..Default::default()
    };
    invoke_plugin(
        &manager,
        invoke(HarnessInput::Start {
            prompt: vec![text_block("go")],
        }),
    )
    .await
    .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "terminal-task".into())
        .await
        .unwrap();
    let request_id = journal["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    invoke_plugin(
        &manager,
        invoke(HarnessInput::Respond {
            request_id,
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
        }),
    )
    .await
    .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "terminal-task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "completed");
    assert!(journal["report"].as_str().unwrap().contains("hello"));
    assert!(manager.acp.lock().unwrap().is_empty());

    let mut bad_policy = binding;
    bad_policy.id = "terminal-bad".into();
    bad_policy.terminal = Some(TerminalPolicy {
        enabled: true,
        allowed_commands: vec![],
        max_seconds: 5,
        max_output_bytes: 1024,
        ..Default::default()
    })
    .into();
    assert_eq!(
        manager.bind(bad_policy).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    manager.shutdown().await.unwrap();
}
