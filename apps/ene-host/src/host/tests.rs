//! Behaviour tests for the headless host. The model is scripted; no external
//! service is called.

use super::{
    Runtime, Store,
    agent::AgentKey,
    chat, client,
    companion::Companion,
    db::{new_id, now},
    error::HostError,
    memory::Memory,
    mind,
    model::{Binding, ModelSource, Purpose},
    settings,
    work::{self, WorkStatus},
};
use ene_rpc as w;
use rig_core::{
    catalog::Catalog,
    operation::Completion,
    serve::{ErasedHandler, adapters::ModelAdapter},
    test_utils::{MockCompletionModel, MockStreamEvent as E},
};
use rig_ecs::{effects::Handler, model::Connection};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

struct Scripted {
    parent: MockCompletionModel,
    worker: MockCompletionModel,
    revisions: AtomicU64,
}

impl ModelSource for Scripted {
    fn connect(
        &self,
        store: &Store,
        companion_id: &str,
        purpose: Purpose,
    ) -> super::error::Result<(Connection, Binding)> {
        store
            .companion(companion_id)?
            .ok_or_else(|| HostError::not_found("companion not found"))?;
        let model = if purpose == Purpose::Task {
            self.worker.clone()
        } else {
            self.parent.clone()
        };
        wrap_model(model, self.revisions.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

fn wrap_model(
    model: MockCompletionModel,
    revision: u64,
) -> super::error::Result<(Connection, Binding)> {
    let spec = std::sync::Arc::new(
        (*Catalog::builtin()
            .resolve("openai/gpt-6-luna")
            .map_err(|_| HostError::unavailable("test model unavailable"))?
            .shared())
        .clone()
        .with_context_window(100_000)
        .with_max_output_tokens(1_000),
    );
    Ok((
        Connection {
            handler: Handler(ErasedHandler::new(ModelAdapter::<Completion>::new(
                spec.reference(),
                model,
            ))),
            spec,
        },
        Binding {
            connection: "test".into(),
            model: "gpt-6-luna".into(),
            revision,
            permit: Arc::new(AtomicBool::new(true)),
        },
    ))
}

struct ScriptedSource(Scripted);

impl ModelSource for ScriptedSource {
    fn connect(
        &self,
        store: &Store,
        companion_id: &str,
        purpose: Purpose,
    ) -> super::error::Result<(Connection, Binding)> {
        self.0.connect(store, companion_id, purpose)
    }
}

fn scripted(
    parent: MockCompletionModel,
    worker: MockCompletionModel,
    consolidate: bool,
) -> super::Options {
    super::Options {
        models: Some(Arc::new(ScriptedSource(Scripted {
            parent,
            worker,
            revisions: AtomicU64::new(0),
        }))),
        consolidate,
        ..Default::default()
    }
}

fn text_model(text: &'static str) -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([vec![
        E::text(text),
        E::final_response_with_default_usage(),
    ]])
}

fn parent_model(turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>) -> MockCompletionModel {
    MockCompletionModel::from_stream_turns(turns)
}

struct NoModel;

impl ModelSource for NoModel {
    fn connect(
        &self,
        _: &Store,
        _: &str,
        _: Purpose,
    ) -> super::error::Result<(Connection, Binding)> {
        Err(HostError::unavailable("no model in this restart"))
    }
}

async fn default_companion(runtime: &Runtime) -> String {
    runtime
        .call(|app| {
            Ok(app
                .world()
                .resource::<Store>()
                .companions()?
                .first()
                .map(|companion| companion.id.clone())
                .unwrap_or_default())
        })
        .await
        .unwrap()
}

async fn send(runtime: &Runtime, companion: &str, key: &str, text: &str) -> w::Acceptance {
    let companion = companion.to_owned();
    let key = key.to_owned();
    let text = text.to_owned();
    runtime
        .call(move |app| {
            chat::accept(
                app,
                w::SendMessageRequest {
                    companion_id: companion,
                    request_key: key,
                    text,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap()
}

async fn history(runtime: &Runtime, companion: &str, include_context: bool) -> w::History {
    let companion = companion.to_owned();
    runtime
        .call(move |app| {
            chat::history(
                app.world_mut(),
                w::HistoryRequest {
                    companion_id: companion,
                    limit: Some(200),
                    include_context: Some(include_context),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap()
}

async fn until(runtime: &Runtime, companion: &str, text: &str) -> w::History {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let seen = history(runtime, companion, true).await;
            if seen.items.iter().any(|message| message.text == text) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host did not answer")
}

fn memory(owner: &str, content: &str) -> Memory {
    Memory {
        id: new_id(),
        owner: owner.into(),
        audiences: Vec::new(),
        content: content.into(),
        description: String::new(),
        appraisal: String::new(),
        acquisition: super::memory::Acquisition::UserStatement,
        importance: 5,
        occurred_at: None,
        recorded_at: now(),
        last_accessed_at: None,
        memory_revision: 1,
        content_revision: 1,
        emotion: None,
        source_context: json!([]),
        supersedes: Vec::new(),
        created_by: "test".into(),
        updated_at: now(),
    }
}

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
        super::Options {
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
async fn a_work_session_runs_while_the_parent_keeps_talking_and_reports_once() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call("start", "start_work", json!({"instruction": "整理して"})),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("started"), E::final_response_with_default_usage()],
        vec![
            E::text("作業が終わったよ"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let worker = text_model("done");
    let runtime = Runtime::start_with(temp.path(), scripted(parent, worker, false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "work", "任せる").await;
    until(&runtime, &companion, "started").await;
    let seen = until(&runtime, &companion, "作業が終わったよ").await;
    assert!(
        seen.items
            .iter()
            .any(|message| message.text.contains("done"))
    );
    let works = runtime
        .call(|app| app.world().resource::<Store>().works())
        .await
        .unwrap();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].status, WorkStatus::Idle);
    assert!(works[0].reported_run.as_deref().is_some());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn clients_are_registered_and_activation_is_exclusive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let a = runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client-a".into(),
                    name: "A".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    let b = runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client-b".into(),
                    name: "B".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(a.revision, 1);
    assert_eq!(b.revision, 1);
    let first = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client-a".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(first.client_id.as_deref(), Some("client-a"));
    let second = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client-b".into(),
                        expected_revision: first.revision,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(second.client_id.as_deref(), Some("client-b"));
    assert!(
        runtime
            .call({
                let companion = companion.clone();
                let revision = second.revision + 1;
                move |app| {
                    client::activate(
                        app,
                        w::ActivationRequest {
                            companion_id: companion,
                            client_id: "client-a".into(),
                            expected_revision: revision,
                            ..Default::default()
                        },
                    )
                }
            })
            .await
            .is_err()
    );
    runtime.shutdown().await.unwrap();
}

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
            .recall(
                &companion,
                "りんご",
                super::memory::RecallMode::Automatic,
                10
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .recall(
                &companion,
                "りんご",
                super::memory::RecallMode::Explicit,
                10
            )
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
            settings::update_policy_rules(
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
        .call(|app| settings::pending_approvals(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(pending.items.is_empty());
    assert!(
        runtime
            .call(|app| {
                settings::submit_approval(
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
async fn a_device_request_nobody_answers_becomes_uncertain() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call(
                "device",
                "device_request",
                json!({"capability": "files", "input": {"path": "note.txt"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("after"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(
        temp.path(),
        super::Options {
            models: Some(Arc::new(ScriptedSource(Scripted {
                parent,
                worker: text_model("unused"),
                revisions: AtomicU64::new(0),
            }))),
            device_timeout: Duration::from_millis(30),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client".into(),
                    name: "client".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    runtime
        .call(|app| {
            client::set_capabilities(
                app,
                w::SetCapabilitiesRequest {
                    client_id: "client".into(),
                    capabilities: vec!["files".into()],
                    granted_capabilities: vec!["files".into()],
                    revision: 1,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    send(&runtime, &companion, "device", "端末で確認して").await;
    until(&runtime, &companion, "after").await;
    let devices = runtime
        .call(|app| app.world().resource::<Store>().devices())
        .await
        .unwrap();
    assert_eq!(devices[0].status, super::client::DeviceStatus::Uncertain);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_target_never_spawns_an_agent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let missing = runtime
        .read(|app| {
            chat::history(
                app.world_mut(),
                w::HistoryRequest {
                    companion_id: "missing".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert!(matches!(missing, Err(HostError::NotFound(_))));
    let agents = runtime
        .call(|app| {
            let mut query = app.world_mut().query::<&AgentKey>();
            Ok(query.iter(app.world()).count())
        })
        .await
        .unwrap();
    assert_eq!(agents, 1);
    runtime.shutdown().await.unwrap();
}

#[test]
fn a_foreign_database_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE documents (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .unwrap();
    drop(connection);
    assert!(Store::open(&path).is_err());
}

#[tokio::test]
async fn companion_seed_is_persistent() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let companions = store.companions().unwrap();
    assert_eq!(companions.len(), 1);
    assert_eq!(companions[0].name, "ene");
    drop(store);
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    assert_eq!(store.companions().unwrap().len(), 1);
}

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
    let options = super::Options {
        models: Some(Arc::new(ScriptedSource(Scripted {
            parent,
            worker: text_model("unused"),
            revisions: AtomicU64::new(0),
        }))),
        consolidate: false,
        compaction: super::compaction::Options {
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
        super::Options {
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
                Ok(app
                    .world()
                    .get::<super::compaction::Summary>(entity)
                    .is_some())
            }
        })
        .await
        .unwrap();
    assert!(restored, "the summary snapshot must survive a restart");
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
            .recall(
                &companion,
                "りんご",
                super::memory::RecallMode::Explicit,
                10
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .recall(
                &companion,
                "ぶどう",
                super::memory::RecallMode::Explicit,
                10
            )
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .recall(&other, "ぶどう", super::memory::RecallMode::Explicit, 10)
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
            .recall(&other, "ぶどう", super::memory::RecallMode::Explicit, 10)
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

#[tokio::test]
async fn work_questions_become_pending_inquiries_and_answers_resolve_them() {
    let temp = tempfile::tempdir().unwrap();
    let worker = parent_model(vec![
        vec![
            E::tool_call(
                "ask",
                "report",
                json!({"kind": "question", "content": "どちらにしますか"}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("了解"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(text_model("unused"), worker, false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "inquiry".into(),
                    "確認して".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let seen = history(&runtime, &companion, true).await;
            if seen
                .items
                .iter()
                .any(|message| message.text.contains("作業からの質問"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the question did not reach the parent");
    let view = runtime
        .call({
            let id = work.id.clone();
            move |app| work::view(app.world_mut(), &id)
        })
        .await
        .unwrap();
    assert_eq!(view.pending_inquiries.len(), 1);
    let request_id = view.pending_inquiries[0].request_id.clone();
    runtime
        .call({
            let id = work.id.clone();
            move |app| {
                work::answer_inquiry(
                    app,
                    w::AnswerInquiryRequest {
                        work_id: id,
                        request_id,
                        answer: "左".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let view = runtime
        .call({
            let id = work.id.clone();
            move |app| work::view(app.world_mut(), &id)
        })
        .await
        .unwrap();
    assert!(view.pending_inquiries.is_empty());
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
            settings::update_policy_rules(
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
                .call(|app| settings::pending_approvals(app.world().resource::<Store>()))
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
            settings::submit_approval(
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
        .call(|app| settings::policy_rules(app.world().resource::<Store>()))
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
        .call(|app| settings::pending_approvals(app.world().resource::<Store>()))
        .await
        .unwrap();
    assert!(pending.items.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_work_replays_the_same_request_key_without_delivering_twice() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("unused"), text_model("done"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "replay".into(),
                    "始めて".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    let send = |key: &'static str| {
        let id = work.id.clone();
        move |app: &mut rig_ecs::prelude::App| {
            work::send(
                app,
                w::SendWorkRequest {
                    work_id: id,
                    request_key: key.into(),
                    text: "追加".into(),
                    ..Default::default()
                },
            )
        }
    };
    let first = runtime.call(send("work-key")).await.unwrap();
    let second = runtime.call(send("work-key")).await.unwrap();
    assert_eq!(first.request_id, second.request_id);
    let conflicting = runtime
        .call({
            let id = work.id.clone();
            move |app: &mut rig_ecs::prelude::App| {
                work::send(
                    app,
                    w::SendWorkRequest {
                        work_id: id,
                        request_key: "work-key".into(),
                        text: "違う内容".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await;
    assert!(conflicting.is_err());
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
            settings::update_policy_rules(
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
                .call(|app| settings::pending_approvals(app.world().resource::<Store>()))
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
                .call(|app| settings::pending_approvals(app.world().resource::<Store>()))
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
        super::Options {
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
async fn stopping_a_closed_work_from_the_tool_keeps_it_closed() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("unused"), text_model("done"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "closed".into(),
                    "止める".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    runtime
        .call({
            let id = work.id.clone();
            move |app| work::close(app, &id)
        })
        .await
        .unwrap();
    let status = runtime
        .call({
            let companion = companion.clone();
            let id = work.id.clone();
            move |app| {
                let agent = chat::find_agent(app.world_mut(), &AgentKey::parent(&companion))
                    .ok_or_else(|| HostError::not_found("agent not found"))?;
                work::stop_tool(app.world_mut(), agent, &id, None)?;
                Ok(app.world().resource::<Store>().work(&id)?.unwrap().status)
            }
        })
        .await
        .unwrap();
    assert_eq!(status, WorkStatus::Closed);
    runtime.shutdown().await.unwrap();
}
