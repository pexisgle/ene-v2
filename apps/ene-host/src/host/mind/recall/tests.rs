use super::*;
use crate::host::settings::{
    Connection, ConnectionConfig, ConnectionKind, CredentialState, EmbeddingConnection, Endpoint,
};
use crate::host::{mind::ModelAssignment, model::ModelFactory, resources::HostRuntime};
use rig_core::{
    catalog::Catalog,
    test_utils::{MockCompletionModel, MockStreamEvent},
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn context_fit_does_not_charge_tools_the_model_cannot_receive() {
    fn offer_large_tool(mut calls: Query<&mut ModelRequest, Added<Admitted>>) {
        for mut call in &mut calls {
            call.request_mut()
                .tools
                .push(Arc::new(rig_core::completion::ToolDefinition::new(
                    rig_core::message::ToolName::new("large_schema").unwrap(),
                    "x".repeat(64_000),
                    serde_json::json!({"type": "object"}),
                )));
        }
    }

    use crate::host::tests::{Harness, text_turn};
    let mut spec = Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .shared()
        .as_ref()
        .clone();
    spec.context_window = Some(8_192);
    spec.max_output_tokens = Some(256);
    spec.tools = false;
    let model = MockCompletionModel::from_stream_turns([text_turn("short answer")]);
    let observed = model.clone();
    let spec = Arc::new(spec);
    let mut harness = Harness::new([]);
    harness
        .app
        .insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
            Ok(model.clone().priced_as(spec.clone()).into())
        })));
    harness
        .app
        .add_systems(AgentPass, offer_large_tool.in_set(PrepareSystems::Edit));
    harness.accept("adapted-budget", "short question").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            harness.tick();
            if let Some(request) = observed.requests().first() {
                assert_eq!(request.tools.len(), 0);
                assert!(rig_memory::request_estimate(request) < 8_192 - 256 - 819);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a fitting adapted request reaches the model");
}

#[tokio::test]
async fn a_slow_embedding_holds_the_model_call_without_blocking_the_host() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let released = release.clone();
    let (started, requested) = tokio::sync::oneshot::channel();
    let endpoint = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        let _discarded = started.send(());
        released.notified().await;
        let body = r#"{"data":[{"embedding":[1.0,0.0],"index":0}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::text("answered"),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let spec = Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .shared();
    let mut harness = crate::host::tests::Harness::new([]);
    let probe = model.clone();
    harness
        .app
        .insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
            Ok(model.clone().priced_as(spec.clone()).into())
        })));
    harness
        .app
        .insert_resource(HostRuntime(tokio::runtime::Handle::current()));
    let world = harness.app.world_mut();
    let id = world
        .query::<&CompanionAgent>()
        .iter(world)
        .next()
        .unwrap()
        .companion_id
        .clone();
    let store = world.resource::<Store>().clone();
    let connection = Connection {
        id: ene_domain::ConnectionId::parse("embedding-test").unwrap(),
        name: "embedding".into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "custom".into(),
                endpoint: format!("http://{address}"),
                options_schema: String::new(),
                options: String::new(),
            },
            model_id: "test-embedding".into(),
            dimensions: 2,
        }),
    };
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(connection, 0)],
        )
        .unwrap();
    let mut companion = store.companion_repo().companion(&id).unwrap().unwrap();
    companion.assignments.embedding = Some(ModelAssignment {
        connection_id: "embedding-test".into(),
        model_id: "test-embedding".into(),
    });
    store
        .companion_repo()
        .update_companion(&companion, companion.revision)
        .unwrap();
    harness.accept("async-tail", "hello").unwrap();
    tokio::time::timeout(Duration::from_secs(2), requested)
        .await
        .unwrap()
        .unwrap();
    assert!(
        probe.requests().is_empty(),
        "the model waits for semantic recall"
    );
    harness.app.update();
    assert!(
        probe.requests().is_empty(),
        "an ECS frame still runs while recall waits"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            harness.app.update();
            if !probe.requests().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the admitted model call resumes after recall");
    endpoint.await.unwrap();
}

#[tokio::test]
async fn work_uses_the_brief_without_the_companions_automatic_recall() {
    use crate::host::db::memories::MemoryRecord;
    use crate::host::tests::{Harness, bound, text_turn};

    for use_caller_runtime in [false, true] {
        let model = MockCompletionModel::from_stream_turns([
            text_turn("head reply"),
            text_turn("work reply"),
        ]);
        let probe = model.clone();
        let mut harness = Harness::new([]);
        harness
            .app
            .insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
                Ok(bound(&model))
            })));
        if use_caller_runtime {
            harness
                .app
                .insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        }
        let store = harness.store();
        let head_memory = store
            .memory_repo()
            .insert_memory(&MemoryRecord {
                id: "m-head".into(),
                scope: harness.companion.clone(),
                content: "コンパニオン用の紅茶の記憶".into(),
                importance: 7,
                occurred_at: None,
                source_context: serde_json::json!({}),
            })
            .unwrap();
        store
            .memory_repo()
            .insert_memory(&MemoryRecord {
                id: "m-work".into(),
                scope: harness.companion.clone(),
                content: "フォルダ整理手順 と関係のない保管先を開かないこと".into(),
                importance: 7,
                occurred_at: None,
                source_context: serde_json::json!({}),
            })
            .unwrap();

        let head_entity = harness.companion_agent();
        harness
            .accept("k-head", "紅茶の記憶 について教えて")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                harness.tick();
                let answered = harness
                    .conversation(head_entity)
                    .entries()
                    .any(|entry| matches!(entry.message, Message::Assistant(_)));
                if probe.requests().len() == 1 && answered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the companion turn finishes");

        let receipts = |companion_id: &str| {
            store
                .memory_repo()
                .injected_between(companion_id, None, u64::MAX)
                .unwrap()
        };
        let expected = [ene_mind::MemoryVersion::from(&head_memory)];
        assert_eq!(receipts(&harness.companion), expected);

        let companion = store
            .companion_repo()
            .companion(&harness.companion)
            .unwrap()
            .unwrap();
        let work_entity =
            agent::spawn_work_agent(harness.app.world_mut(), &companion, "work-1", "run-1");
        let brief = "フォルダ整理手順 の必要事項: PDF を月別に整理する";
        assert!(agent::deliver(
            harness.app.world_mut(),
            work_entity,
            &harness.companion,
            brief.into(),
            DeliveryMode::Queue,
            Author::user(),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                harness.tick();
                let answered = harness
                    .conversation(work_entity)
                    .entries()
                    .any(|entry| matches!(entry.message, Message::Assistant(_)));
                if probe.requests().len() == 2 && answered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the work turn finishes");

        let work_request = format!("{:?}", probe.requests()[1]);
        assert_eq!(work_request.matches(brief).count(), 1, "{work_request}");
        assert!(!work_request.contains("関係のない保管先"), "{work_request}");
        assert!(
            !work_request.contains(&head_memory.content),
            "{work_request}"
        );
        assert_eq!(
            receipts(&harness.companion),
            expected,
            "work agent turns must not pollute companion memory_injections"
        );
    }
}

#[tokio::test]
async fn b13_turn_cause_uses_schedule_instruction_instead_of_stale_user_chat() {
    use crate::host::db::memories::MemoryRecord;
    use crate::host::tests::{Harness, bound, text_turn};

    let model = MockCompletionModel::from_stream_turns([
        text_turn("紅茶だね"),
        text_turn("会議の時間だね"),
    ]);
    let probe = model.clone();
    let mut harness = Harness::new([]);
    harness
        .app
        .insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
            Ok(bound(&model))
        })));
    let store = harness.store();
    store
        .memory_repo()
        .insert_memory(&MemoryRecord {
            id: "m-tea".into(),
            scope: harness.companion.clone(),
            content: "ユーザーは アールグレイ紅茶 が大好き".into(),
            importance: 8,
            occurred_at: None,
            source_context: serde_json::json!({}),
        })
        .unwrap();
    store
        .memory_repo()
        .insert_memory(&MemoryRecord {
            id: "m-standup".into(),
            scope: harness.companion.clone(),
            content: "月曜の 定例朝会 は議事録係を毎回交代する".into(),
            importance: 8,
            occurred_at: None,
            source_context: serde_json::json!({}),
        })
        .unwrap();

    let head = harness.companion_agent();
    harness
        .accept("k-tea", "アールグレイ紅茶 の話しよう")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            harness.tick();
            if probe.requests().len() == 1
                && harness
                    .conversation(head)
                    .entries()
                    .any(|e| agent::entry_text(&e).is_some_and(|t| t.contains("紅茶だね")))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first user turn settles");

    // Now deliver a scheduled turn (NOT a user message): recall must match
    // the schedule instruction ("定例朝会"), NOT the stale user chat ("アールグレイ紅茶").
    assert!(agent::deliver(
        harness.app.world_mut(),
        head,
        &harness.companion,
        "[スケジュール実行: 朝会] 定例朝会 の準備をして".into(),
        DeliveryMode::Queue,
        Author::plugin("ene-host"),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            harness.tick();
            if probe.requests().len() == 2
                && harness
                    .conversation(head)
                    .entries()
                    .any(|e| agent::entry_text(&e).is_some_and(|t| t.contains("会議の時間だね")))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scheduled turn settles");

    let second_req = format!("{:?}", probe.requests()[1]);
    assert!(
        second_req.contains("月曜の 定例朝会 は議事録係を毎回交代する"),
        "scheduled turn must recall memory for its own instruction: {second_req}"
    );
    assert!(
        !second_req.contains("ユーザーは アールグレイ紅茶 が大好き"),
        "scheduled turn must not recycle stale user query from earlier turn: {second_req}"
    );
}

#[test]
fn b14_context_builder_marks_only_complete_memories_that_fit_budget() {
    use crate::host::db::memories::MemoryRecord;
    use crate::host::tests::Harness;

    let harness = Harness::new([]);
    let store = harness.store();
    let first_content = format!("予算テスト記憶A {}", "あ".repeat(90));
    let second_content = format!("予算テスト記憶B {}", "い".repeat(300));
    store
        .memory_repo()
        .insert_memory(&MemoryRecord {
            id: "m-fit".into(),
            scope: harness.companion.clone(),
            content: first_content.clone(),
            importance: 9,
            occurred_at: None,
            source_context: serde_json::json!({}),
        })
        .unwrap();
    store
        .memory_repo()
        .insert_memory(&MemoryRecord {
            id: "m-dropped".into(),
            scope: harness.companion.clone(),
            content: second_content.clone(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        })
        .unwrap();

    // Measure base priority tail without recall, then set a tight budget
    // that fits the header + `m-fit` cleanly while dropping `m-dropped`.
    let base = render_tail_full(&TailRenderRequest {
        store: &store,
        perception: &Perception::default(),
        companion: &harness.companion,
        query: None,
        semantic: &[],
        hook_notes: &[],
        budget_chars: TAIL_BUDGET_CHARS,
    })
    .unwrap();
    let base = base.payload(&store, &harness.companion).unwrap();
    let budget = base.text.chars().count()
        + "今の話に関係する記憶:\n".chars().count()
        + first_content.chars().count()
        + 20;

    let tail = render_tail_full(&TailRenderRequest {
        store: &store,
        perception: &Perception::default(),
        companion: &harness.companion,
        query: Some("予算テスト記憶"),
        semantic: &[],
        hook_notes: &[],
        budget_chars: budget,
    })
    .unwrap();
    let tail = tail.payload(&store, &harness.companion).unwrap();
    assert!(tail.text.chars().count() <= budget);
    assert!(
        tail.text.contains(&first_content),
        "first recalled memory fits"
    );
    assert!(
        !tail.text.contains(&second_content),
        "second recalled memory must be omitted when over budget"
    );
    let recorded = store
        .memory_repo()
        .injected_between(&harness.companion, None, u64::MAX)
        .unwrap();
    assert_eq!(
        tail.memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        ["m-fit"],
        "rendering witnesses only memories that survived the tail budget"
    );
    assert_eq!(recorded.len(), 0, "rendering alone never writes a receipt");
}

#[test]
fn pinned_memories_over_the_budget_remain_eligible_for_ranked_recall() {
    let harness = crate::host::tests::Harness::new([]);
    let store = harness.store();
    for index in 0..7 {
        store
            .memory_repo()
            .insert_memory(&ene_mind::MemoryRecord {
                id: format!("pin-{index}"),
                scope: harness.companion.clone(),
                content: format!("Memory {index}. {}", "blue cupboard ".repeat(31)),
                importance: 10,
                occurred_at: None,
                source_context: serde_json::Value::Null,
            })
            .unwrap();
    }
    let tail = render_tail_full(&TailRenderRequest {
        store: &store,
        perception: &Perception::default(),
        companion: &harness.companion,
        query: Some("cupboard"),
        semantic: &[],
        hook_notes: &[],
        budget_chars: TAIL_BUDGET_CHARS,
    })
    .unwrap()
    .payload(&store, &harness.companion)
    .unwrap();
    let (pinned, ranked) = tail.text.split_once("今の話に関係する記憶:\n").unwrap();
    assert!(pinned.contains("Memory 6."));
    assert!(!pinned.contains("Memory 0."));
    assert!(ranked.contains("Memory 0."));
    assert_eq!(tail.memories.len(), 7);
    assert!((0..7).all(|index| tail.text.matches(&format!("Memory {index}.")).count() == 1));
}

#[test]
fn a_cached_tail_omits_revoked_and_partial_pinned_memories() {
    use crate::host::tests::Harness;

    let harness = Harness::new([]);
    let store = harness.store();
    let repo = store.memory_repo();
    let perception = Perception::default();
    let mut req = TailRenderRequest {
        store: &store,
        perception: &perception,
        companion: &harness.companion,
        query: None,
        semantic: &[],
        hook_notes: &[],
        budget_chars: TAIL_BUDGET_CHARS,
    };
    let base = render_tail_full(&req)
        .unwrap()
        .payload(&store, &harness.companion)
        .unwrap();
    req.budget_chars = base.text.chars().count() + 104;
    let insert = |id: &str, scope: &str, content: String| {
        repo.insert_memory(&ene_mind::MemoryRecord {
            id: id.into(),
            scope: scope.into(),
            content,
            importance: 10,
            occurred_at: None,
            source_context: serde_json::Value::Null,
        })
        .unwrap()
    };
    let _oversized = insert("too-long", &harness.companion, "secret-partial ".repeat(25));
    let revoked = insert("revoked", "shared", "private-on-revoke".into());
    let _remaining = insert("safe", &harness.companion, "still-authorized".into());
    let cached = render_tail_full(&req).unwrap();
    let initial = cached.payload(&store, &harness.companion).unwrap();
    assert!(
        initial.text.contains("private-on-revoke") && initial.text.contains("still-authorized")
    );
    assert!(!initial.text.contains("secret-partial"));
    repo.control_memory(&revoked.id, ene_mind::MemoryActionKind::Unshare, "other")
        .unwrap();
    let next = cached.payload(&store, &harness.companion).unwrap();
    assert!(!next.text.contains("private-on-revoke"));
    assert!(next.text.contains("still-authorized"));
    assert_eq!(
        next.memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        ["safe"]
    );
}

#[tokio::test]
async fn only_a_confirmed_model_request_records_injected_memories() {
    use crate::host::db::memories::MemoryRecord;
    use crate::host::tests::{Harness, text_turn};

    for fits in [false, true] {
        let mut harness = Harness::new([]);
        let model = MockCompletionModel::from_stream_turns([text_turn("reply")]);
        let observed = model.clone();
        let mut spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .unwrap()
            .spec
            .clone();
        spec.context_window = Some(4_096);
        spec.max_output_tokens = Some(256);
        spec.tools = false;
        let spec = Arc::new(spec);
        harness
            .app
            .insert_resource(ModelFactory(Arc::new(move |_, _, _, _, _, _| {
                Ok(model.clone().priced_as(spec.clone()).into())
            })));
        let store = harness.store();
        for (id, content, importance) in [
            ("matching", "purple bananas must stay in the bowl", 7),
            ("pinned", "The fixed companion's favorite day is Friday", 10),
        ] {
            store
                .memory_repo()
                .insert_memory(&MemoryRecord {
                    id: id.into(),
                    scope: harness.companion.clone(),
                    content: content.into(),
                    importance,
                    occurred_at: None,
                    source_context: serde_json::json!({}),
                })
                .unwrap();
        }
        let text = if fits {
            "purple bananas".into()
        } else {
            format!("purple bananas {}", "長".repeat(6_000))
        };
        let head = harness.companion_agent();
        harness.accept("confirmed-injection", &text).unwrap();
        harness.run_until(|world| {
            world.get::<Conversation>(head).is_some_and(|conversation| {
                conversation
                    .turns()
                    .next_back()
                    .is_some_and(|(_, turn)| turn.end.is_some())
            })
        });
        assert_eq!(observed.requests().len(), usize::from(fits));
        let receipts = store
            .memory_repo()
            .injected_between(&harness.companion, None, u64::MAX)
            .unwrap();
        if fits {
            assert_eq!(
                receipts
                    .into_iter()
                    .map(|memory| memory.id)
                    .collect::<Vec<_>>(),
                ["pinned", "matching"]
            );
        } else {
            assert_eq!(receipts.len(), 0);
        }
    }
}

#[test]
fn q05_storage_fault_in_tail_marks_host_failed_instead_of_empty_state() {
    use crate::host::tests::{Harness, text_turn};

    let mut harness = Harness::new([text_turn("ok")]);
    // Drop `user_profiles` to simulate a SQLite storage fault during recall.
    harness
        .store()
        .conn()
        .execute("DROP TABLE user_profiles", [])
        .unwrap();
    let err = render_tail_full(&TailRenderRequest {
        store: &harness.store(),
        perception: &Perception::default(),
        companion: &harness.companion,
        query: Some("query"),
        semantic: &[],
        hook_notes: &[],
        budget_chars: TAIL_BUDGET_CHARS,
    })
    .unwrap_err();
    assert!(err.is_storage(), "expected storage error, got {err:?}");
    let _discarded = harness.accept("k-fault", "こんにちは");
    harness.run_until(|world| {
        world
            .resource::<crate::host::HostState>()
            .failure()
            .is_some()
    });
}
