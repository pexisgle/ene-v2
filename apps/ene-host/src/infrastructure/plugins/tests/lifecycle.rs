use super::*;
use ene_plugin::Operation;
use std::time::Duration;

async fn invoke(manager: &Manager, input: HarnessInput) -> Result<PluginResult> {
    invoke_plugin(
        manager,
        PluginInvocation {
            binding_id: "task".into(),
            operation: document(serde_json::to_value(Operation::Harness { input }).unwrap()).into(),
            ..Default::default()
        },
    )
    .await
}

fn start(prompt: &str) -> HarnessInput {
    HarnessInput::Start {
        prompt: vec![ene_plugin::ContentBlock::text(prompt)],
    }
}
fn allow() -> HarnessInput {
    HarnessInput::Respond {
        request_id: "permission-1".into(),
        outcome: ene_plugin::RequestPermissionOutcome::Selected {
            option_id: "allow".into(),
        },
    }
}

async fn pending() -> (Manager, tempfile::TempDir, PluginBinding) {
    let (manager, temp, companion) = manager().await;
    let binding = binding(&manager, &companion, "task", "harness", vec![]);
    manager
        .enable(
            "reference".into(),
            EnablePluginRequest {
                enabled: true,
                revision: binding.revision.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager.bind(binding.clone()).await.unwrap();
    invoke(&manager, start("test")).await.unwrap();
    (manager, temp, binding)
}

#[tokio::test]
async fn shutdown_waits_for_disable_after_the_rpc_waiter_is_dropped() {
    let (manager, _temp, binding) = pending().await;
    let app = application(&manager);
    let session = manager.sessions.lock().unwrap()["task"].clone();
    let held = session.worker.lock().await;
    let other = app.clone();
    let waiter = tokio::spawn(async move {
        other
            .enable_plugin(
                "reference".into(),
                EnablePluginRequest {
                    enabled: false,
                    revision: binding.revision,
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), session.lifetime.cancelled())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let other = app.clone();
    let mut stop = tokio::spawn(async move { other.stop().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut stop)
            .await
            .is_err()
    );
    drop(held);
    tokio::time::timeout(Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let saved = manager
        .store
        .plugin_get("enabled".into(), "reference".into())
        .await
        .unwrap();
    let saved: EnablePluginRequest = serde_json::from_value(saved["input"].clone()).unwrap();
    assert!(!saved.enabled);
    assert_eq!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap()["status"],
        "failed"
    );
    assert!(manager.sessions.lock().unwrap().is_empty());
    assert_eq!(
        app.reload_plugins().await.unwrap_err().kind,
        ErrorKind::Unavailable
    );
}

#[tokio::test]
async fn rejected_harness_inputs_preserve_the_pending_guest_and_journal() {
    let (manager, _temp, _) = pending().await;
    let session = manager.sessions.lock().unwrap()["task"].clone();
    let generation = session.worker.lock().await.as_ref().unwrap().generation();
    let before = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    for (input, kind) in [
        (start("duplicate"), ErrorKind::StateConflict),
        (
            HarnessInput::Respond {
                request_id: "wrong-id".into(),
                outcome: ene_plugin::RequestPermissionOutcome::Cancelled,
            },
            ErrorKind::StateConflict,
        ),
        (
            HarnessInput::Respond {
                request_id: "permission-1".into(),
                outcome: ene_plugin::RequestPermissionOutcome::Selected {
                    option_id: "unknown".into(),
                },
            },
            ErrorKind::InvalidRequest,
        ),
    ] {
        assert_eq!(invoke(&manager, input).await.unwrap_err().kind, kind);
        assert_eq!(
            manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap(),
            before
        );
        assert!(Arc::ptr_eq(
            &manager.sessions.lock().unwrap()["task"],
            &session
        ));
    }
    let result = invoke(&manager, allow()).await.unwrap();
    assert_eq!(result.generation, generation);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn rebind_finishes_the_old_harness_before_starting_another() {
    let (manager, _temp, mut binding) = pending().await;
    let old = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    binding.configuration = document(json!({"changed": true})).into();
    manager.bind(binding.clone()).await.unwrap();
    assert!(manager.sessions.lock().unwrap().is_empty());
    let saved = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(saved["session_id"], old["session_id"]);
    assert_eq!(saved["status"], "failed");
    assert_eq!(saved["failure"], "plugin_stopped");
    assert!(saved["pending"].is_null());
    assert_eq!(
        saved["events"].as_array().unwrap().len(),
        old["events"].as_array().unwrap().len()
    );
    manager.bind(binding).await.unwrap();
    assert_eq!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap(),
        saved
    );
    invoke(&manager, start("new task")).await.unwrap();
    assert_ne!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap()["session_id"],
        old["session_id"]
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_storage_failures_are_reported_and_retriable() {
    for ending in [
        "disable", "rebind", "reload", "shutdown", "cancel", "invoke",
    ] {
        let (manager, _temp, mut binding) = pending().await;
        let app = application(&manager);
        let before = manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        manager.store.run(|db| {
            db.execute_batch("CREATE TEMP TRIGGER reject_harness BEFORE UPDATE ON plugin_records WHEN OLD.namespace='harness' AND NEW.value != OLD.value BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;")?;
            Ok(())
        }).await.unwrap();
        let result = match ending {
            "disable" => {
                manager
                    .enable(
                        "reference".into(),
                        EnablePluginRequest {
                            enabled: false,
                            revision: binding.revision.clone(),
                            ..Default::default()
                        },
                    )
                    .await
            }
            "rebind" => {
                binding.configuration = document(json!({"changed": true})).into();
                manager.bind(binding).await
            }
            "reload" => manager.reload().await,
            "shutdown" => app.stop().await,
            "cancel" => invoke(&manager, HarnessInput::Cancel).await.map(|_| ()),
            "invoke" => invoke(&manager, allow()).await.map(|_| ()),
            _ => unreachable!(),
        };
        assert_eq!(
            result.unwrap_err().kind,
            ErrorKind::StorageFailure,
            "{ending}"
        );
        assert_eq!(
            manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap(),
            before
        );
        let session = manager.sessions.lock().unwrap()["task"].clone();
        if ending != "invoke" {
            assert!(session.lifetime.is_cancelled());
            assert!(session.worker.lock().await.is_none());
        }
        if ending == "rebind" {
            assert_eq!(
                manager.bindings().await.unwrap()[0].configuration.json,
                "{}"
            );
        }
        manager
            .store
            .run(|db| {
                db.execute_batch("DROP TRIGGER reject_harness")?;
                Ok(())
            })
            .await
            .unwrap();
        app.stop().await.unwrap();
        assert!(manager.sessions.lock().unwrap().is_empty());
        let saved = manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        assert_eq!(saved["status"], "failed");
        assert_eq!(saved["failure"], "plugin_stopped");
        assert!(saved["pending"].is_null());
        assert_eq!(
            saved["events"].as_array().unwrap().len(),
            before["events"].as_array().unwrap().len()
        );
        app.stop().await.unwrap();
        assert_eq!(
            manager
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap(),
            saved
        );
    }
}

async fn fail_writes(manager: &Manager) {
    manager.store.run(|db| {
        db.execute_batch("CREATE TEMP TRIGGER reject_harness BEFORE UPDATE ON plugin_records WHEN OLD.namespace='harness' BEGIN SELECT RAISE(ABORT, 'review write failure'); END;")?;
        Ok(())
    }).await.unwrap();
}
async fn recover_writes(manager: &Manager) {
    manager
        .store
        .run(|db| {
            db.execute_batch("DROP TRIGGER reject_harness")?;
            Ok(())
        })
        .await
        .unwrap();
}
fn cancel_request() -> PluginInvocation {
    PluginInvocation {
        binding_id: "task".into(),
        operation: document(json!({"type":"harness","input":{"type":"cancel"}})).into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn cancel_can_be_retried_after_storage_recovers() {
    let (manager, _temp, _) = pending().await;
    let app = application(&manager);
    fail_writes(&manager).await;
    assert_eq!(
        app.invoke_plugin(cancel_request()).await.unwrap_err().kind,
        ErrorKind::StorageFailure
    );
    recover_writes(&manager).await;
    let retry = app.invoke_plugin(cancel_request()).await.unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "cancelled");
    assert!(journal["pending"].is_null());
    assert!(manager.sessions.lock().unwrap().is_empty());
    let confirmed = app.invoke_plugin(cancel_request()).await.unwrap();
    assert_eq!(retry.result.json, confirmed.result.json);
    assert_eq!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap(),
        journal
    );
    assert_eq!(
        app.stop().await.unwrap_err().kind,
        ErrorKind::StorageFailure
    );
}

async fn reopen(manager: &Manager, temp: &tempfile::TempDir) -> Manager {
    let mut restarted =
        Manager::discover(Store::open(temp.path().join("host.sqlite3")).await.unwrap());
    restarted.root = manager.root.clone();
    restarted.entries.write().unwrap().clear();
    restarted.scan().unwrap();
    restarted
}

#[tokio::test]
async fn rebind_recovers_a_task_after_failed_shutdown_and_restart() {
    for recover_at_start in [false, true] {
        let (manager, _temp, binding) = pending().await;
        let app = application(&manager);
        fail_writes(&manager).await;
        assert_eq!(
            app.stop().await.unwrap_err().kind,
            ErrorKind::StorageFailure
        );
        recover_writes(&manager).await;
        let restarted = reopen(&manager, &_temp).await;
        drop(app);
        drop(manager);
        let app = application(&restarted);
        if recover_at_start {
            app.restore_plugins().await.unwrap();
            let journal = restarted
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap();
            assert_eq!(journal["status"], "failed");
            assert_eq!(journal["failure"], "host_interrupted");
        } else {
            restarted
                .enable(
                    "reference".into(),
                    EnablePluginRequest {
                        enabled: true,
                        revision: binding.revision.clone(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert!(restarted.ready("reference", None).is_ok());
        restarted.bind(binding).await.unwrap();
        let journal = restarted
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        assert_eq!(journal["status"], "failed");
        assert!(journal["pending"].is_null());
        let retry = invoke(&restarted, start("after restart")).await;
        app.stop().await.unwrap();
        assert!(
            retry.is_ok(),
            "rebind after restart left an orphan task: {:?}, journal={}",
            retry.err(),
            journal["status"]
        );
    }
}

#[tokio::test]
async fn wrong_extension_does_not_destroy_pending_harness() {
    let (manager, _temp, binding) = pending().await;
    let app = application(&manager);
    let before = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    let session = manager.sessions.lock().unwrap()["task"].clone();
    let generation = session.worker.lock().await.as_ref().unwrap().generation();
    let rejected = app
        .invoke_plugin(PluginInvocation {
            binding_id: binding.id,
            operation: document(
                serde_json::to_value(Operation::Character {
                    target: CharacterTarget {
                        device_id: binding.device_id,
                        character_id: binding.companion_id,
                    },
                    event: CharacterEvent::Activated,
                })
                .unwrap(),
            )
            .into(),
            ..Default::default()
        })
        .await;
    assert_eq!(rejected.unwrap_err().kind, ErrorKind::UnsupportedCapability);
    assert!(Arc::ptr_eq(
        &manager.sessions.lock().unwrap()["task"],
        &session
    ));
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal, before);
    let retry = invoke(&manager, allow()).await.unwrap();
    assert_eq!(retry.generation, generation);
    app.stop().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_guest_state_write() {
    for fail_write in [false, true] {
        let (manager, temp, companion) = manager().await;
        let binding = binding(
            &manager,
            &companion,
            "behavior",
            "behavior",
            vec![Service::State],
        );
        manager
            .enable(
                "reference".into(),
                EnablePluginRequest {
                    enabled: true,
                    revision: binding.revision.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        manager.bind(binding.clone()).await.unwrap();
        let app = application(&manager);
        app.initialize().await.unwrap();
        if fail_write {
            manager.store.run(|db| {
            db.execute_batch("CREATE TEMP TRIGGER reject_state BEFORE INSERT ON plugin_records WHEN NEW.namespace LIKE 'state:%' BEGIN SELECT RAISE(ABORT, 'injected state write failure'); END;")?;
            Ok(())
        }).await.unwrap();
        }
        let db =
            tokio_rusqlite::rusqlite::Connection::open(temp.path().join("host.sqlite3")).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let other = app.clone();
        let request = tokio::spawn(async move {
            other
                .invoke_plugin(PluginInvocation {
                    binding_id: binding.id,
                    operation: document(
                        serde_json::to_value(Operation::Character {
                            target: CharacterTarget {
                                device_id: binding.device_id,
                                character_id: binding.companion_id,
                            },
                            event: CharacterEvent::Activated,
                        })
                        .unwrap(),
                    )
                    .into(),
                    ..Default::default()
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if tokio::time::timeout(
                    Duration::from_millis(100),
                    manager.store.plugin_get("probe".into(), "probe".into()),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        })
        .await
        .expect("guest must reach SQLite's blocked write");
        let mut stopping = Box::pin(app.stop());
        let early = tokio::time::timeout(Duration::from_millis(200), &mut stopping)
            .await
            .is_ok();
        db.execute_batch("ROLLBACK").unwrap();
        assert!(!early, "shutdown must wait for the admitted state write");
        let result = tokio::time::timeout(Duration::from_secs(3), stopping)
            .await
            .unwrap();
        if fail_write {
            assert_eq!(result.unwrap_err().kind, ErrorKind::StorageFailure);
        } else {
            result.unwrap();
        }
        assert!(request.await.unwrap().is_err());
        let saved = manager
            .store
            .plugin_get(
                format!("state:reference:behavior:{companion}"),
                "visits".into(),
            )
            .await
            .unwrap();
        assert_eq!(saved, if fail_write { Value::Null } else { json!(1) });
    }
}

#[tokio::test]
async fn startup_recovery_reports_storage_failure_and_preserves_terminal_tasks() {
    for terminal in [false, true] {
        let (manager, temp, _) = pending().await;
        if terminal {
            invoke(&manager, HarnessInput::Cancel).await.unwrap();
        }
        let before = manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        let restarted = reopen(&manager, &temp).await;
        drop(manager);
        let app = application(&restarted);
        fail_writes(&restarted).await;
        let restored = app.restore_plugins().await;
        if terminal {
            restored.unwrap();
        } else {
            assert_eq!(restored.unwrap_err().kind, ErrorKind::StorageFailure);
            assert!(restarted.ready("reference", None).is_err());
        }
        assert_eq!(
            restarted
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap(),
            before
        );
        recover_writes(&restarted).await;
        app.restore_plugins().await.unwrap();
        let recovered = restarted
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap();
        if terminal {
            assert_eq!(recovered, before);
        } else {
            assert_eq!(recovered["status"], "failed");
            assert_eq!(recovered["failure"], "host_interrupted");
            assert!(recovered["pending"].is_null());
        }
        app.restore_plugins().await.unwrap();
        assert_eq!(
            restarted
                .store
                .plugin_get("harness".into(), "task".into())
                .await
                .unwrap(),
            recovered
        );
        app.stop().await.unwrap();
    }
}

#[tokio::test]
async fn respond_without_a_live_guest_is_rejected() {
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
    let mut journal = HarnessSession::start("orphan");
    let call = HarnessEvent::Update {
        update: SessionUpdate::ToolCall(ene_plugin::ToolCall {
            tool_call_id: "call-1".into(),
            title: "step".into(),
            ..Default::default()
        }),
    };
    manager
        .harness_apply(&binding.id, &mut journal, &call)
        .await
        .unwrap();
    let permission = HarnessEvent::RequestPermission {
        request_id: "permission-1".into(),
        tool_call: ene_plugin::ToolCallUpdate {
            tool_call_id: "call-1".into(),
            ..Default::default()
        },
        options: vec![ene_plugin::PermissionOption {
            option_id: "allow".into(),
            name: "Allow".into(),
            kind: ene_plugin::PermissionOptionKind::AllowOnce,
        }],
    };
    manager
        .harness_apply(&binding.id, &mut journal, &permission)
        .await
        .unwrap();
    let before = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(
        invoke(&manager, allow()).await.unwrap_err().kind,
        ErrorKind::StateConflict
    );
    assert_eq!(
        manager
            .store
            .plugin_get("harness".into(), "task".into())
            .await
            .unwrap(),
        before
    );
    assert!(manager.sessions.lock().unwrap().is_empty());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_denied_permission_fails_the_task() {
    let (manager, _temp, _) = pending().await;
    let result = invoke(
        &manager,
        HarnessInput::Respond {
            request_id: "permission-1".into(),
            outcome: ene_plugin::RequestPermissionOutcome::Selected {
                option_id: "deny".into(),
            },
        },
    )
    .await
    .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(result.events.len(), 1);
    assert_eq!(journal["status"], "failed");
    assert_eq!(journal["failure"], "refusal");
    assert_eq!(journal["stop_reason"], "refusal");
    assert!(journal["pending"].is_null());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_event_that_cannot_be_persisted_fails_the_turn() {
    let (manager, _temp, _) = pending().await;
    manager
        .store
        .run(|db| {
            db.execute_batch("CREATE TEMP TRIGGER reject_report BEFORE UPDATE ON plugin_records WHEN OLD.namespace='harness' AND NEW.value LIKE '%参照タスクを完了%' BEGIN SELECT RAISE(ABORT, 'injected report failure'); END;")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        invoke(&manager, allow()).await.unwrap_err().kind,
        ErrorKind::StorageFailure
    );
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["status"], "failed");
    assert_eq!(journal["failure"], "invocation_failed");
    assert!(journal["pending"].is_null());
    assert!(manager.sessions.lock().unwrap().is_empty());
    manager
        .store
        .run(|db| {
            db.execute_batch("DROP TRIGGER reject_report")?;
            Ok(())
        })
        .await
        .unwrap();
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_legacy_journal_is_finalized_without_resume() {
    let (manager, _temp, _) = pending().await;
    manager
        .store
        .plugin_set(
            "harness".into(),
            "task".into(),
            json!({
                "task_id": "legacy-task",
                "status": "waiting_answer",
                "pending": "question-1",
                "question": true,
                "events": [],
                "report": null
            }),
        )
        .await
        .unwrap();
    manager
        .harness_stop("task", "host_interrupted")
        .await
        .unwrap();
    let journal = manager
        .store
        .plugin_get("harness".into(), "task".into())
        .await
        .unwrap();
    assert_eq!(journal["contract_version"], 4);
    assert_eq!(journal["session_id"], "legacy-task");
    assert_eq!(journal["status"], "failed");
    assert_eq!(journal["failure"], "host_interrupted");
    assert!(journal["pending"].is_null());
    assert!(journal["events"].as_array().unwrap().is_empty());
    manager.shutdown().await.unwrap();
}
