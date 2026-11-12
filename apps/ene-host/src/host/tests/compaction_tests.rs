use super::*;
#[tokio::test]
async fn an_idle_turn_is_compacted_without_rewriting_the_log() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![E::text("ok"), E::final_response_with_default_usage()],
        vec![
            E::text("古い会話の要約"),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("了解"), E::final_response_with_default_usage()],
    ]);
    let requests = parent.clone();
    let policy = rig_memory::CompactionPolicy {
        keep_recent: 1,
        ..Default::default()
    };
    let options = Options {
        models: Some(Arc::new(ScriptedSource(Scripted {
            parent,
            worker: text_model("unused"),
            revisions: AtomicU64::new(0),
        }))),
        consolidate: false,
        compaction: compaction::Options {
            policy,
            idle: Duration::ZERO,
            threshold: 0.0,
        },
        ..Default::default()
    };
    let runtime = Runtime::start_with(temp.path(), options).await.unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "compact", "長い話").await;
    until(&runtime, &companion, "ok").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            if requests.requests().len() >= 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("compaction did not run");
    let visible = history(&runtime, &companion, true).await;
    assert!(
        !visible
            .items
            .iter()
            .any(|message| message.text.contains("古い会話の要約")),
        "the summary is view-only and must not rewrite GetHistory"
    );
    send(&runtime, &companion, "after", "続き").await;
    until(&runtime, &companion, "了解").await;
    let seen = requests.requests();
    let summarized = seen
        .iter()
        .find(|request| {
            let history = format!("{:?}", request.chat_history);
            history.contains("<summary>") && history.contains("古い会話の要約")
        })
        .expect("no request received the view-only summary");
    let history = format!("{:?}", summarized.chat_history);
    assert!(!history.contains("長い話"), "{history}");
    assert!(history.contains("続き"), "{history}");
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
    let restored = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                let entity = chat::find_agent(app.world_mut(), &AgentKey::parent(&companion))
                    .ok_or_else(|| HostError::not_found("agent not restored"))?;
                Ok(app.world().get::<compaction::Summary>(entity).is_some())
            }
        })
        .await
        .unwrap();
    assert!(restored, "the summary snapshot must survive a restart");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_user_message_drops_a_running_compaction_and_requeues_it() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let job = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                let agent = chat::find_agent(app.world_mut(), &AgentKey::parent(&companion))
                    .ok_or_else(|| HostError::not_found("agent not found"))?;
                let job = app
                    .world_mut()
                    .spawn((
                        compaction::SummaryJob { keep_from: None },
                        rig_ecs::prelude::ForAgent(agent),
                    ))
                    .id();
                app.world_mut()
                    .resource_mut::<resources::CompactionQueue>()
                    .0
                    .insert(companion.clone());
                Ok(job)
            }
        })
        .await
        .unwrap();
    send(&runtime, &companion, "interrupt", "話しかけた").await;
    let cancelled = runtime
        .call(move |app| Ok(app.world().get_entity(job).is_err()))
        .await
        .unwrap();
    assert!(cancelled, "the running summary job must be dropped");
    let queued = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                Ok(app
                    .world()
                    .resource::<resources::CompactionQueue>()
                    .0
                    .contains(&companion))
            }
        })
        .await
        .unwrap();
    assert!(
        queued,
        "the summary must be deferred to the next idle period"
    );
    runtime.shutdown().await.unwrap();
}
