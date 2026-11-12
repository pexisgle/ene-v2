use super::*;
#[tokio::test]
async fn settings_policies_and_approvals_are_revisioned() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let (settings, _) = runtime
        .call(|app| {
            settings::update_settings(
                app.world_mut(),
                w::UpdateSettingsRequest {
                    expected_revision: 1,
                    connections: vec![w::ConnectionInput {
                        id: "conn".into(),
                        enabled: true,
                        kind: "llm".into(),
                        adapter: "openai".into(),
                        endpoint: "https://api.openai.com/v1".into(),
                        options: "{}".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(settings.revision, 2);
    assert_eq!(settings.connections.len(), 1);
    assert!(!settings.updated_at.is_empty());

    let rules = runtime
        .call(|app| {
            permission::update_policy_rules(
                app.world_mut(),
                w::UpdatePolicyRulesRequest {
                    add_or_update: vec![w::PolicyRule {
                        id: "rule-1".into(),
                        tool_name: "shell".into(),
                        pattern: "rm -rf".into(),
                        effect: "confirm".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(rules.items.len(), 1);
    assert_eq!(rules.items[0].effect, "confirm");
    let pending = runtime
        .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(pending.items.is_empty());
    assert!(
        runtime
            .call(|app| {
                permission::submit_approval(
                    app.world_mut(),
                    w::SubmitApprovalRequest {
                        request_id: "missing".into(),
                        decision: "always_approve".into(),
                        ..Default::default()
                    },
                )
            })
            .await
            .is_err()
    );

    let (settings, forgotten) = runtime
        .call(|app| {
            settings::update_settings(
                app.world_mut(),
                w::UpdateSettingsRequest {
                    expected_revision: 2,
                    connections: Vec::new(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(settings.revision, 3);
    assert!(settings.connections.is_empty());
    assert!(forgotten.is_empty());
    let stale = runtime
        .call(|app| {
            settings::update_settings(
                app.world_mut(),
                w::UpdateSettingsRequest {
                    expected_revision: 2,
                    connections: Vec::new(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert!(stale.is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_confirm_policy_emits_an_approval_request_before_the_tool_runs() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call(
                "dev",
                "device_request",
                json!({"capability": "files", "input": {"path": "x"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("unused"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("unused"), false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            permission::update_policy_rules(
                app.world_mut(),
                w::UpdatePolicyRulesRequest {
                    add_or_update: vec![w::PolicyRule {
                        id: "confirm-device".into(),
                        tool_name: "device_request".into(),
                        effect: "confirm".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    send(&runtime, &companion, "confirm", "端末を使って").await;
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let pending = runtime
                .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
                .await
                .unwrap();
            if let Some(request) = pending.items.first() {
                return request.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no approval request");
    assert_eq!(approval.tool_name, "device_request");
    assert_eq!(approval.status, "pending");
    let approval_id = approval.request_id.clone();
    runtime
        .call(move |app| {
            permission::submit_approval(
                app.world_mut(),
                w::SubmitApprovalRequest {
                    request_id: approval_id.clone(),
                    decision: "always_approve".into(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    let rules = runtime
        .call(|app| permission::policy_rules(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(
        !rules
            .items
            .iter()
            .any(|rule| rule.effect == "always_approve"),
        "always_approve must not bypass an explicit confirm rule"
    );
    let request_id = approval.request_id.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let current = runtime
                .call({
                    let id = request_id.clone();
                    move |app| app.world().resource::<Store>().approval(&id)
                })
                .await
                .unwrap()
                .unwrap();
            if current.status == "consumed" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approved tool call did not resume and consume the approval");
    let pending = runtime
        .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(pending.items.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_denied_approval_refuses_the_tool_without_running_it() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call(
                "dev",
                "device_request",
                json!({"capability": "files", "input": {"path": "x"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("denied"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("unused"), false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            permission::update_policy_rules(
                app.world_mut(),
                w::UpdatePolicyRulesRequest {
                    add_or_update: vec![w::PolicyRule {
                        id: "confirm-device".into(),
                        tool_name: "device_request".into(),
                        effect: "confirm".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    send(&runtime, &companion, "deny", "端末を使って").await;
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let pending = runtime
                .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
                .await
                .unwrap();
            if let Some(request) = pending.items.first() {
                return request.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no approval request");
    let request_id = approval.request_id.clone();
    runtime
        .call(move |app| {
            permission::submit_approval(
                app.world_mut(),
                w::SubmitApprovalRequest {
                    request_id,
                    decision: "deny".into(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    until(&runtime, &companion, "denied").await;
    let devices = runtime
        .call(|app| app.world().resource::<Store>().devices())
        .await
        .unwrap();
    assert!(devices.is_empty(), "a denied tool must not run");
    let stored = runtime
        .call({
            let id = approval.request_id.clone();
            move |app| app.world().resource::<Store>().approval(&id)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, "denied");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stopping_an_approval_does_not_reuse_its_hold_for_the_next_call() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call(
                "dev-a",
                "device_request",
                json!({"capability": "files", "input": {"path": "x"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![
            E::tool_call(
                "dev-b",
                "device_request",
                json!({"capability": "files", "input": {"path": "x"}}),
            ),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("unused"), false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            permission::update_policy_rules(
                app.world_mut(),
                w::UpdatePolicyRulesRequest {
                    add_or_update: vec![w::PolicyRule {
                        id: "confirm-device".into(),
                        tool_name: "device_request".into(),
                        effect: "confirm".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    send(&runtime, &companion, "held", "端末を使って").await;
    let first_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let pending = runtime
                .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
                .await
                .unwrap();
            if let Some(first) = pending.items.first() {
                return first.request_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first approval was not created");
    runtime
        .call({
            let companion = companion.clone();
            move |app| chat::stop(app, &companion)
        })
        .await
        .unwrap();
    let _ = runtime.call(|_| Ok(())).await;
    let old = runtime
        .call({
            let id = first_id.clone();
            move |app| app.world().resource::<Store>().approval(&id)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.status, "expired");
    send(&runtime, &companion, "held-again", "もう一度").await;
    let pending = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let pending = runtime
                .call(|app| permission::pending_approvals(app.world().resource::<Store>()))
                .await
                .unwrap();
            if pending.items.len() == 1 && pending.items[0].request_id != first_id {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stopped approval was reused instead of creating a new one");
    assert_eq!(pending.items.len(), 1);
    assert_eq!(pending.items[0].status, "pending");
    runtime.shutdown().await.unwrap();
}
