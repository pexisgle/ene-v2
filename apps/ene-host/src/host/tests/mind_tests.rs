use super::*;
#[tokio::test]
async fn a_mind_update_is_stored_and_the_next_turn_recalls_it() {
    let temp = tempfile::tempdir().unwrap();
    let model = text_model("りんごだね");
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(model.clone(), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let memory = memory(&companion, "りんごが好き");
    let saved = memory.clone();
    runtime
        .call(move |app| app.world().resource::<Store>().save_memory(&saved))
        .await
        .unwrap();
    send(&runtime, &companion, "fruit", "りんご").await;
    until(&runtime, &companion, "りんごだね").await;
    let requests = model.requests();
    assert!(!requests.is_empty());
    let seen = format!("{:?}", requests[0].chat_history);
    assert!(seen.contains("関連する記憶"), "{seen}");
    assert!(seen.contains("りんごが好き"), "{seen}");
    let visible = history(&runtime, &companion, true).await;
    assert!(
        !visible
            .items
            .iter()
            .any(|message| message.text.contains("関連する記憶")),
        "dynamic context must not be persisted"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn memory_control_forgets_shares_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let memory = memory(&companion, "青森のりんご");
    let id = memory.id.clone();
    let saved = memory.clone();
    runtime
        .call(move |app| app.world().resource::<Store>().save_memory(&saved))
        .await
        .unwrap();

    let forgotten = runtime
        .call({
            let companion = companion.clone();
            let id = id.clone();
            move |app| {
                mind::control_memory(
                    app.world_mut(),
                    w::MemoryControlRequest {
                        companion_id: companion,
                        memory_id: id,
                        action: "forget".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(forgotten.importance, 0);
    let store = runtime
        .call(|app| Ok(app.world().resource::<Store>().clone()))
        .await
        .unwrap();
    assert!(
        store
            .recall(&companion, "りんご", mind::RecallMode::Automatic, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .recall(&companion, "りんご", mind::RecallMode::Explicit, 10)
            .unwrap()
            .len(),
        1
    );
    let shared = runtime
        .call({
            let companion = companion.clone();
            let id = id.clone();
            move |app| {
                mind::control_memory(
                    app.world_mut(),
                    w::MemoryControlRequest {
                        companion_id: companion,
                        memory_id: id,
                        action: "share".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(shared.audiences, ["*"]);
    let again = runtime
        .call({
            let companion = companion.clone();
            let id = id.clone();
            move |app| {
                mind::control_memory(
                    app.world_mut(),
                    w::MemoryControlRequest {
                        companion_id: companion,
                        memory_id: id,
                        action: "share".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(again.revision, shared.revision);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_settled_turn_is_consolidated_into_memory() {
    let temp = tempfile::tempdir().unwrap();
    let extraction = json!({
        "reason": "remembered",
        "memories": [{
            "content": "りんごが好き",
            "description": "好物",
            "appraisal": "うれしい",
            "acquisition": "user_statement",
            "importance": 5
        }]
    })
    .to_string();
    let parent = parent_model(vec![
        vec![E::text("ok"), E::final_response_with_default_usage()],
        vec![
            E::text(extraction.clone()),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("unused"), true))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "first", "りんごが好き").await;
    until(&runtime, &companion, "ok").await;
    let wait_runtime = runtime.clone();
    let wait_companion = companion.clone();
    tokio::time::timeout(Duration::from_secs(5), async move {
        loop {
            let _ = wait_runtime.call(|_| Ok(())).await;
            let companion = wait_companion.clone();
            let count = wait_runtime
                .call(move |app| app.world().resource::<Store>().count_memories(&companion))
                .await
                .unwrap();
            if count == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consolidation did not store a memory");
    let memories = runtime
        .call(move |app| app.world().resource::<Store>().memories_for(&companion))
        .await
        .unwrap();
    assert_eq!(memories[0].content, "りんごが好き");
    assert_eq!(memories[0].importance, 5);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn mind_update_is_idempotent_and_stores_importance() {
    let temp = tempfile::tempdir().unwrap();
    let update = json!({
        "expected_revision": 1,
        "memories": [{
            "content": "りんごが好き",
            "acquisition": "user_statement",
            "importance": 7
        }]
    });
    let parent = parent_model(vec![
        vec![
            E::tool_call("mem", "mind_update", update.clone()),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("覚えた"), E::final_response_with_default_usage()],
        vec![
            E::tool_call("mem", "mind_update", update),
            E::final_response_with_default_usage(),
        ],
        vec![
            E::text("もう覚えてる"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("unused"), false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "one", "りんごが好き").await;
    until(&runtime, &companion, "覚えた").await;
    send(&runtime, &companion, "two", "りんごが好きって言った").await;
    until(&runtime, &companion, "もう覚えてる").await;
    let memories = runtime
        .call(move |app| app.world().resource::<Store>().memories_for(&companion))
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].importance, 7);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn memory_fts_tracks_updates_and_respects_audiences() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let other = runtime
        .call(|app| {
            let store = app.world().resource::<Store>();
            let mut record = Companion::seed(new_id(), now());
            record.name = "other".into();
            store.save_companion(&record)?;
            Ok(record.id)
        })
        .await
        .unwrap();
    let mut item = memory(&companion, "青森のりんご");
    let id = item.id.clone();
    let saved = item.clone();
    runtime
        .call(move |app| app.world().resource::<Store>().save_memory(&saved))
        .await
        .unwrap();
    item.content = "長野のぶどう".into();
    let updated = item.clone();
    runtime
        .call(move |app| app.world().resource::<Store>().save_memory(&updated))
        .await
        .unwrap();
    let store = runtime
        .call(|app| Ok(app.world().resource::<Store>().clone()))
        .await
        .unwrap();
    assert!(store.memory(&id).unwrap().is_some());
    assert!(
        store
            .recall(&companion, "りんご", mind::RecallMode::Explicit, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .recall(&companion, "ぶどう", mind::RecallMode::Explicit, 10)
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .recall(&other, "ぶどう", mind::RecallMode::Explicit, 10)
            .unwrap()
            .is_empty()
    );
    let mut shared = item;
    shared.audiences = vec!["*".into()];
    let shared_copy = shared.clone();
    runtime
        .call(move |app| app.world().resource::<Store>().save_memory(&shared_copy))
        .await
        .unwrap();
    assert_eq!(
        store
            .recall(&other, "ぶどう", mind::RecallMode::Explicit, 10)
            .unwrap()
            .len(),
        1
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn companions_store_all_seven_assignments_persona_and_voice() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let created = runtime
        .call(|app| {
            mind::create_companion(
                app.world_mut(),
                w::CreateCompanionRequest {
                    name: "extra".into(),
                    mind: "calm".into(),
                    body: "{}".into(),
                    voice: "{}".into(),
                    assignments: Some(w::ModelAssignments {
                        companion: Some(w::ModelAssignment {
                            connection_id: "c".into(),
                            model_id: "m".into(),
                            ..Default::default()
                        })
                        .into(),
                        perception: Some(w::ModelAssignment {
                            connection_id: "p".into(),
                            model_id: "pm".into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .into(),
                    persona: Some("persona".into()),
                    voice_identifier: Some("voice-1".into()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(created.persona.as_deref(), Some("persona"));
    assert_eq!(created.voice_identifier.as_deref(), Some("voice-1"));
    let updated = runtime
        .call({
            let id = created.id.clone();
            move |app| {
                mind::update_companion(
                    app.world_mut(),
                    w::UpdateCompanionRequest {
                        id,
                        revision: created.revision,
                        perception_assignment: Some(w::AssignmentPatch {
                            change: Some(w::assignment_patch::Change::Set(Box::new(
                                w::ModelAssignment {
                                    connection_id: "p2".into(),
                                    model_id: "pm2".into(),
                                    ..Default::default()
                                },
                            ))),
                            ..Default::default()
                        })
                        .into(),
                        decision_assignment: Some(w::AssignmentPatch {
                            change: Some(w::assignment_patch::Change::Set(Box::new(
                                w::ModelAssignment {
                                    connection_id: "d".into(),
                                    model_id: "dm".into(),
                                    ..Default::default()
                                },
                            ))),
                            ..Default::default()
                        })
                        .into(),
                        persona: Some("changed".into()),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(updated.persona.as_deref(), Some("changed"));
    let response = updated.assignments.as_option().unwrap();
    assert_eq!(response.perception.as_option().unwrap().connection_id, "p2");
    assert_eq!(response.decision.as_option().unwrap().connection_id, "d");
    runtime
        .call({
            let id = updated.id.clone();
            move |app| mind::delete_companion(app.world_mut(), &id)
        })
        .await
        .unwrap();
    let list = runtime
        .call(|app| mind::companions(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(!list.iter().any(|companion| companion.id == updated.id));
    runtime.shutdown().await.unwrap();
}
