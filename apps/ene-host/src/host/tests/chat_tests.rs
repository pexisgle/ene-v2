use super::*;
#[tokio::test]
async fn a_turn_is_accepted_answered_and_restored() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("こんにちは"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let accepted = send(&runtime, &companion, "key", "hi").await;
    assert!(!accepted.request_id.is_empty());
    let seen = until(&runtime, &companion, "こんにちは").await;
    assert!(seen.items.iter().any(|message| message.role == "user"));
    let replay = send(&runtime, &companion, "key", "hi").await;
    assert_eq!(replay.request_id, accepted.request_id);
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let runtime = Runtime::start_with(
        temp.path(),
        Options {
            models: Some(Arc::new(NoModel)),
            consolidate: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let seen = history(&runtime, &companion, false).await;
    assert!(
        seen.items
            .iter()
            .any(|message| message.text == "こんにちは"),
        "history must survive a restart"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reused_request_key_with_different_text_is_a_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "same", "one").await;
    let error = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                chat::accept(
                    app,
                    w::SendMessageRequest {
                        companion_id: companion.clone(),
                        request_key: "same".into(),
                        text: "two".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(error, HostError::AlreadyExists(_)));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_orphan_receipt_does_not_hide_a_later_valid_receipt_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("unused"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                let entity = chat::find_agent(app.world_mut(), &AgentKey::parent(&companion))
                    .ok_or_else(|| HostError::not_found("agent not found"))?;
                chat::record_accepted(
                    app.world_mut(),
                    entity,
                    "orphan-key",
                    "orphan-hash",
                    "missing-input",
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let first = send(&runtime, &companion, "orphan-key", "hello").await;
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let runtime = Runtime::start_with(
        temp.path(),
        Options {
            models: Some(Arc::new(NoModel)),
            consolidate: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let second = send(&runtime, &companion, "orphan-key", "hello").await;
    assert_eq!(first.request_id, second.request_id);
    let conflict = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                chat::accept(
                    app,
                    w::SendMessageRequest {
                        companion_id: companion,
                        request_key: "orphan-key".into(),
                        text: "different".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await;
    assert!(matches!(conflict, Err(HostError::AlreadyExists(_))));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_oversized_input_is_refused_before_it_enters_the_log() {
    let temp = tempfile::tempdir().unwrap();
    let model = text_model("unused");
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(model.clone(), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let text = "あ".repeat(32_001);
    let error = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                chat::accept(
                    app,
                    w::SendMessageRequest {
                        companion_id: companion,
                        request_key: "too-large".into(),
                        text,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(error, HostError::Invalid(_)));
    assert_eq!(model.request_count(), 0);
    let seen = history(&runtime, &companion, true).await;
    assert!(seen.items.is_empty());
    runtime.shutdown().await.unwrap();
}
