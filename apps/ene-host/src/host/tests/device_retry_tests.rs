use super::*;
use ene_domain::{ClientId, DeviceScope, DeviceStatus, RunState};
use serde_json::json;

#[test]
fn queued_work_reconnect_checks_the_original_run_and_current_grants() {
    for change in ["unchanged", "stopped", "revoked"] {
        let mut harness = Harness::new([
            tool_turn(
                "write",
                "device_request",
                json!({"capability": "device.files", "input": {"path": "fixture.txt"}}),
            ),
            text_turn("書き込みの結果を確認しました"),
        ]);
        harness
            .app
            .world_mut()
            .resource_mut::<crate::host::resources::DeviceTimeout>()
            .0 = Duration::from_secs(5);
        register(
            harness.app.world_mut(),
            "desk",
            w::ClientCapabilitySpec {
                approval: w::CapabilityApproval::Auto.into(),
                ..files_capability()
            },
        );
        activate(harness.app.world_mut(), &harness.companion, "desk");
        let session = start_work(&mut harness);
        let store = harness.store();
        let run = store.work_repo().active_run(&session.id).unwrap().unwrap();
        let repo = store.client_repo();
        harness.run_until(|_| !repo.outstanding_operations("desk").unwrap().is_empty());
        let queued = repo.outstanding_operations("desk").unwrap().remove(0);
        assert_eq!(queued.status, DeviceStatus::Pending);
        assert_rejected_result(&mut harness, &queued.id, "desk", DeviceStatus::Pending);
        assert_eq!(
            queued.scope,
            DeviceScope::Work {
                companion_id: ene_domain::CompanionId::parse(&harness.companion).unwrap(),
                work_id: session.id.clone(),
                run_id: run.run_id.clone(),
            }
        );

        if change == "stopped" {
            work::stop(harness.app.world_mut(), &session.id).unwrap();
            assert_eq!(
                store.work_repo().run(&run.run_id).unwrap().unwrap().state,
                RunState::Cancelled
            );
            assert_eq!(
                repo.operation(&queued.id).unwrap().unwrap().status,
                DeviceStatus::Failed
            );
        }
        if change == "revoked" {
            client::declare(
                harness.app.world_mut(),
                client::Declaration::Replace(&w::SetCapabilitiesRequest {
                    client_id: "desk".into(),
                    revision: repo.client("desk").unwrap().unwrap().revision,
                    ..Default::default()
                }),
            )
            .unwrap();
        }
        let hub = harness.app.world().resource::<DeviceHub>().clone();
        let client_id = ClientId::parse("desk").unwrap();
        let (subscription, mut stream) = hub.register(&client_id);
        let context = device::DeviceDispatchContext {
            store: &store,
            hub: &hub,
            notifier: harness.app.world().resource(),
        };
        let mut rejected = Vec::new();
        device::resend(&context, "desk", |id, _| rejected.push(id.to_owned())).unwrap();
        if change == "unchanged" {
            let Some(w::device_event::Event::Operation(sent)) = stream.try_recv().unwrap().event
            else {
                panic!("the live run's operation should be sent");
            };
            assert_eq!(sent.operation_id, queued.id);
            assert_eq!(sent.companion_id, harness.companion);
            assert_eq!(sent.run_id.as_deref(), Some(run.run_id.as_str()));
            assert_eq!(
                repo.operation(&queued.id).unwrap().unwrap().status,
                DeviceStatus::Dispatched
            );
            assert_rejected_result(
                &mut harness,
                &queued.id,
                "other-client",
                DeviceStatus::Dispatched,
            );
            assert!(hub.unregister(&client_id, subscription));
            let late = device::complete(
                harness.app.world_mut(),
                &w::DeviceOperationResult {
                    client_id: "desk".into(),
                    operation_id: queued.id.clone(),
                    success: true,
                    output: "{}".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(late.status, w::DeviceOperationStatus::Uncertain);
        } else {
            assert!(
                stream.try_recv().is_err(),
                "{change} operations cannot start"
            );
            assert_eq!(
                repo.operation(&queued.id).unwrap().unwrap().status,
                DeviceStatus::Failed
            );
        }
        if change == "revoked" {
            assert_eq!(rejected, [queued.id]);
        } else {
            assert_eq!(rejected, [] as [String; 0]);
        }
    }
}

fn assert_rejected_result(
    harness: &mut Harness,
    operation_id: &str,
    client_id: &str,
    expected: DeviceStatus,
) {
    let result = w::DeviceOperationResult {
        client_id: client_id.into(),
        operation_id: operation_id.into(),
        success: true,
        output: "{}".into(),
        ..Default::default()
    };
    let error = device::complete(harness.app.world_mut(), &result).unwrap_err();
    match expected {
        DeviceStatus::Pending => assert!(matches!(error, crate::host::HostError::Conflict(_))),
        DeviceStatus::Dispatched => assert!(matches!(error, crate::host::HostError::Permission(_))),
        _ => panic!("fixture expected an unfinished operation"),
    }
    assert_eq!(
        harness
            .store()
            .client_repo()
            .operation(operation_id)
            .unwrap()
            .unwrap()
            .status,
        expected
    );
}
