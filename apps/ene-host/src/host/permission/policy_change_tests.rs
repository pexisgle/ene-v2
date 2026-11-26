use super::*;

#[test]
fn cancelling_a_held_call_expires_the_card_and_cancels_evaluation() {
    for human_card in [true, false] {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve("unused", STALL));
        let mut world = DecisionWorld::at(&runtime, &provider.base, &decision_companion());
        if human_card {
            world
                .store()
                .policy_repo()
                .apply_policy_rules(
                    &[PolicyRule {
                        id: "confirm".into(),
                        tool_name: "device_request".into(),
                        pattern: "*".into(),
                        effect: PolicyEffect::Confirm,
                        created_at: String::new(),
                    }],
                    &[],
                )
                .unwrap();
        }
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        assert_eq!(
            world.app.world().resource::<Decisions>().running.len(),
            usize::from(!human_card)
        );
        assert_eq!(cards(&world.events()).len(), usize::from(human_card));
        assert!(world.app.world_mut().despawn(call));
        world.app.update();
        let approval = world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Expired);
        assert_eq!(notices(&world.events(), "approval_resolved"), 1);
        assert!(world.app.world().resource::<Decisions>().running.is_empty());
        provider.task.abort();
    }
}

#[test]
fn a_decision_reply_based_on_old_policy_cannot_release_a_call() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"allow","reason":"safe under the previous policy"}"#,
        Duration::from_secs(1),
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, &decision_companion());
    let call = world.gated_call("/tmp/x");
    world.app.update();
    let request = world.pending().request_id;
    assert_eq!(world.app.world().resource::<Decisions>().running.len(), 1);
    world
        .store()
        .policy_repo()
        .apply_policy_texts(
            &[PolicyText {
                id: "policy-changed".into(),
                text: "ファイル操作は毎回確認する".into(),
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    settle(&mut world.app, &runtime);
    assert_eq!(provider.bodies.lock().len(), 1);
    assert_eq!(cards(&world.events()), vec![request]);
    assert_eq!(world.pending().status, ApprovalStatus::Pending);
    assert!(world.app.world().get::<Approved>(call).is_none());
    provider.task.abort();
}

#[test]
fn a_saved_deny_blocks_operator_approval_on_the_same_client() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve("unused", STALL));
    let mut world = DecisionWorld::at(&runtime, &provider.base, &decision_companion());
    let call = world.gated_call("/tmp/x");
    world.app.update();
    let request = world.pending().request_id;
    world
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[PolicyRule {
                id: "deny-x".into(),
                tool_name: "device.files".into(),
                pattern: "/tmp/x".into(),
                effect: PolicyEffect::Deny,
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();
    assert!(matches!(
        submit(world.app.world_mut(), &request, ApprovalDecision::Approve),
        Err(HostError::Permission(_))
    ));
    let approval = world
        .store()
        .approval_repo()
        .approval(&request)
        .unwrap()
        .unwrap();
    assert_eq!(approval.status, ApprovalStatus::Denied);
    assert!(world.app.world().get::<Approved>(call).is_none());
    assert_eq!(
        world
            .store()
            .client_repo()
            .outstanding_operations("c1")
            .unwrap(),
        [] as [ene_domain::DeviceOperation; 0]
    );
    provider.task.abort();
}

#[test]
fn b06_submit_keeps_hold_when_bound_client_loses_grant() {
    let runtime = paused_runtime();
    let provider = runtime.block_on(serve(
        r#"{"decision":"require_approval","reason":"ask"}"#,
        Duration::ZERO,
    ));
    let mut world = DecisionWorld::at(&runtime, &provider.base, &decision_companion());
    let call = world.gated_call("/tmp/a");
    world.app.update();
    let request = world.pending().request_id;
    world
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[PolicyRule {
                id: "deny-files-grant".into(),
                tool_name: "client_capability".into(),
                pattern: "device.files".into(),
                effect: PolicyEffect::Deny,
                created_at: String::new(),
            }],
            &[],
        )
        .unwrap();

    let err = submit(world.app.world_mut(), &request, ApprovalDecision::Approve).unwrap_err();
    assert!(matches!(err, HostError::Conflict(_)));
    assert_eq!(
        world
            .store()
            .approval_repo()
            .approval(&request)
            .unwrap()
            .unwrap()
            .status,
        ApprovalStatus::Pending
    );
    assert!(
        world
            .app
            .world()
            .resource::<Approvals>()
            .by_request
            .contains_key(&request),
        "hold remains intact after pre-commit failure"
    );
    assert!(world.app.world().get::<Approved>(call).is_none());
    provider.task.abort();
}
