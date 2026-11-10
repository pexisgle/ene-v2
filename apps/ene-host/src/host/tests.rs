//! Behaviour tests for the headless host. The model is scripted; no external
//! service is called.

use super::*;
use crate::host::{
    chat, client,
    mind::{self, CompanionRecord},
    model::Binding,
    settings,
    work::WorkRecord,
};
use ene_rpc as w;
use rig_core::{
    catalog::Catalog,
    operation::Completion,
    serve::{ErasedHandler, adapters::ModelAdapter},
    test_utils::{MockCompletionModel, MockStreamEvent as E},
};
use rig_ecs::{effects::Handler, model::Connection};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

/// A scripted model source: the parent's script, the work agents' script,
/// and a revision that rises so `bind` re-binds as it would on a change.
struct Scripted {
    parent: MockCompletionModel,
    worker: MockCompletionModel,
    revisions: AtomicU64,
}

impl model::ModelSource for Scripted {
    fn connect(
        &self,
        host: &Host,
        companion_id: &str,
        purpose: model::Purpose,
    ) -> Result<(Connection, Binding)> {
        mind::companion(&host.db, companion_id)?;
        let model = if purpose == model::Purpose::Task {
            self.worker.clone()
        } else {
            self.parent.clone()
        };
        wrap_model(model, self.revisions.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

fn wrap_model(model: MockCompletionModel, revision: u64) -> Result<(Connection, Binding)> {
    let spec = Catalog::builtin()
        .resolve("openai/gpt-6-luna")
        .map_err(|_| ConnectError::unavailable("test model unavailable"))?
        .shared();
    wrap_spec(model, spec, revision)
}

fn wrap_spec(
    model: MockCompletionModel,
    spec: Arc<rig_core::catalog::ModelSpec>,
    revision: u64,
) -> Result<(Connection, Binding)> {
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

/// A source whose model advertises the window and output limit a test needs.
struct Windowed {
    model: MockCompletionModel,
    window: Option<u32>,
    output: Option<u32>,
}

impl model::ModelSource for Windowed {
    fn connect(
        &self,
        host: &Host,
        companion_id: &str,
        _: model::Purpose,
    ) -> Result<(Connection, Binding)> {
        mind::companion(&host.db, companion_id)?;
        let spec = if self.window.is_none() && self.output.is_none() {
            rig_core::catalog::ModelSpec::new(
                rig_core::providers::registry::ProviderId::catalog("openai")
                    .ok_or_else(|| ConnectError::unavailable("test provider is unknown"))?,
                "gpt-6-luna",
            )
            .with_tools(true)
        } else {
            let mut spec = (*Catalog::builtin()
                .resolve("openai/gpt-6-luna")
                .map_err(|_| ConnectError::unavailable("test model unavailable"))?
                .shared())
            .clone();
            if let Some(window) = self.window {
                spec = spec.with_context_window(window);
            }
            if let Some(output) = self.output {
                spec = spec.with_max_output_tokens(output);
            }
            spec
        };
        wrap_spec(self.model.clone(), Arc::new(spec), 1)
    }
}

fn windowed(
    model: MockCompletionModel,
    window: Option<u32>,
    output: Option<u32>,
    consolidate: bool,
) -> Options {
    Options {
        models: Some(Arc::new(Windowed {
            model,
            window,
            output,
        })),
        consolidate,
        ..Options::default()
    }
}

fn scripted(
    parent: MockCompletionModel,
    worker: MockCompletionModel,
    consolidate: bool,
) -> Options {
    Options {
        models: Some(Arc::new(Scripted {
            parent,
            worker,
            revisions: AtomicU64::new(0),
        })),
        consolidate,
        ..Options::default()
    }
}

fn model_options(model: MockCompletionModel) -> Options {
    scripted(model.clone(), model, false)
}

fn text_model(text: &'static str) -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([vec![
        E::text(text),
        E::final_response_with_default_usage(),
    ]])
}

fn factory(text: &'static str) -> Options {
    let model = text_model(text);
    scripted(model.clone(), model, false)
}

/// A source that refuses to connect, as a host without a model would.
struct NoModel;

impl model::ModelSource for NoModel {
    fn connect(&self, _: &Host, _: &str, _: model::Purpose) -> Result<(Connection, Binding)> {
        Err(ConnectError::unavailable("no model in this restart"))
    }
}

fn no_model() -> Options {
    Options {
        models: Some(Arc::new(NoModel)),
        consolidate: false,
        ..Options::default()
    }
}

fn target(companion: &str) -> w::Target {
    w::Target {
        companion_id: companion.into(),
        ..Default::default()
    }
}

async fn default_companion(runtime: &Runtime) -> String {
    runtime
        .call(|app| {
            Ok(mind::companions(&app.world().resource::<Host>().db)?
                .into_iter()
                .next()
                .unwrap()
                .id)
        })
        .await
        .unwrap()
}

async fn send(runtime: &Runtime, companion: &str, key: &str, text: &str) -> w::MessageAcceptance {
    let companion = companion.to_owned();
    let key = key.to_owned();
    let text = text.to_owned();
    runtime
        .call(move |app| {
            chat::accept(
                app,
                w::SendMessageRequest {
                    target: target(&companion).into(),
                    request_key: key,
                    text,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap()
}

async fn history(runtime: &Runtime, target: w::Target) -> w::History {
    runtime
        .call(move |app| {
            chat::history(
                app.world_mut(),
                w::HistoryRequest {
                    target: target.into(),
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
            let seen = history(runtime, target(companion)).await;
            if seen.items.iter().any(|message| message.text == text) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host did not answer")
}

#[tokio::test]
async fn a_turn_is_accepted_answered_and_restored() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("こんにちは"))
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

    let runtime = Runtime::start_with(temp.path(), no_model()).await.unwrap();
    let seen = history(&runtime, target(&companion)).await;
    assert!(
        seen.items
            .iter()
            .any(|message| message.text == "こんにちは"),
        "history must survive a restart"
    );
    runtime.shutdown().await.unwrap();
}

#[test]
fn a_companion_record_round_trips_its_json_fields() {
    let record = CompanionRecord::seed("id".into(), "now".into());
    let encoded = serde_json::to_string(&record).unwrap();
    let decoded: CompanionRecord = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.companion.id, record.companion.id);
    assert_eq!(decoded.profile, record.profile);
    assert_eq!(decoded.state, record.state);
}

#[tokio::test]
async fn a_work_session_runs_while_the_parent_keeps_talking_and_reports_once() {
    let temp = tempfile::tempdir().unwrap();
    let parent = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "start",
                "start_work",
                serde_json::json!({"prompt": "整理して"}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("started"), E::final_response_with_default_usage()],
    ]);
    let worker = MockCompletionModel::from_stream_turns([vec![
        E::text("done"),
        E::final_response_with_default_usage(),
    ]]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, worker, false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "work", "任せる").await;
    let seen = until(&runtime, &companion, "started").await;
    assert!(seen.items.iter().any(|message| message.text == "started"));
    let report = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let seen = history(&runtime, target(&companion)).await;
            if seen
                .items
                .iter()
                .any(|message| message.text.contains("作業") && message.text.contains("done"))
            {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("work report did not arrive");
    assert!(
        report
            .items
            .iter()
            .any(|message| message.text.contains("done"))
    );
    let works = runtime
        .call(move |app| {
            Ok(app
                .world()
                .resource::<Host>()
                .db
                .list::<WorkRecord>("work:")?
                .into_iter()
                .map(|(_, record)| record.session)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].status, "answered");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn clients_are_registered_and_activation_is_exclusive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
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
    let model = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "mem",
                "mind_update",
                serde_json::json!({
                    "expected_revision": 1,
                    "memories": [{
                        "content": "りんごが好き",
                        "acquisition": "user_statement"
                    }]
                }),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("覚えた"), E::final_response_with_default_usage()],
        vec![
            E::text("りんごだね"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), model_options(model))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    for (key, text, answer) in [
        ("first", "りんごが好き", "覚えた"),
        ("second", "りんごが好きって言ってたね", "りんごだね"),
    ] {
        send(&runtime, &companion, key, text).await;
        until(&runtime, &companion, answer).await;
    }
    let view = runtime
        .call({
            let companion = companion.clone();
            move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
        })
        .await
        .unwrap();
    assert_eq!(view.index.as_option().unwrap().memories, 1);
    let seen = context_history(&runtime, target(&companion)).await;
    let note = seen
        .items
        .iter()
        .find(|message| message.text.contains("関連する記憶"))
        .expect("the recall note must be in the streamed context");
    assert_eq!(note.role, "context", "injected context is not the user");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_device_operation_waits_for_the_client_and_returns_its_result() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "device",
                "device_request",
                serde_json::json!({
                    "capability": "files",
                    "input": {"path": "note.txt"}
                }),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![
            E::text("受け取った"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), model_options(model))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "device".into(),
                    name: "phone".into(),
                    platform: "android".into(),
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
                    client_id: "device".into(),
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
                        client_id: "device".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let mut stream = runtime
        .call(|app| client::subscribe(app, "device"))
        .await
        .unwrap();
    let _ = stream.pending;
    send(&runtime, &companion, "device", "note.txt を読んで").await;
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.receiver.recv())
        .await
        .expect("device operation was not dispatched")
        .expect("device stream closed");
    assert_eq!(operation.capability, "files");
    runtime
        .call({
            let operation = operation.operation_id.clone();
            move |app| {
                client::complete(
                    app,
                    w::DeviceOperationResult {
                        client_id: "device".into(),
                        operation_id: operation,
                        success: true,
                        output: w::JsonDocument {
                            json: "{\"ok\":true}".into(),
                            ..Default::default()
                        }
                        .into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    until(&runtime, &companion, "受け取った").await;
    let state = runtime
        .call({
            let operation = operation.operation_id;
            move |app| client::get_operation(app, &operation)
        })
        .await
        .unwrap();
    assert_eq!(state.status, "completed");
    runtime.shutdown().await.unwrap();
}

/// A terminal that never answers must not keep the turn open: the next
/// message would be steered into a turn that cannot end.
#[tokio::test]
async fn a_device_request_nobody_answers_does_not_hold_the_turn_forever() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "device",
                "device_request",
                serde_json::json!({"capability": "files", "input": {"path": "note.txt"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![
            E::text("端末が応答しません"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let mut options = model_options(model);
    options.device_timeout = Duration::from_millis(50);
    let runtime = Runtime::start_with(temp.path(), options).await.unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "device".into(),
                    name: "phone".into(),
                    platform: "android".into(),
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
                    client_id: "device".into(),
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
                        client_id: "device".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let mut stream = runtime
        .call(|app| client::subscribe(app, "device"))
        .await
        .unwrap();
    let _ = stream.pending;
    send(&runtime, &companion, "device", "note.txt を読んで").await;
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.receiver.recv())
        .await
        .expect("device operation was not dispatched")
        .expect("device stream closed");
    // No `complete`: the host gives up on its own and the turn carries on.
    until(&runtime, &companion, "端末が応答しません").await;
    let state = runtime
        .call({
            let operation = operation.operation_id;
            move |app| client::get_operation(app, &operation)
        })
        .await
        .unwrap();
    assert_eq!(state.status, "uncertain");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_settled_turn_is_consolidated_into_memory() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![E::text("first"), E::final_response_with_default_usage()],
        vec![
            E::text(
                "{\"reason\":\"好みを覚えた\",\"memories\":[{\"content\":\"紅茶が好き\",\"description\":\"好み\",\"appraisal\":\"うれしい\",\"acquisition\":\"user_statement\"}]}",
            ),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(model.clone(), model, true))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "memory", "紅茶が好き").await;
    until(&runtime, &companion, "first").await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime
                .call({
                    let companion = companion.clone();
                    move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                })
                .await
                .unwrap();
            if view
                .index
                .as_option()
                .is_some_and(|index| index.memories > 0)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consolidation did not store a memory");
    assert_eq!(view.index.as_option().unwrap().memories, 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_and_grants_are_revisioned_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), Options::default())
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    let connection = w::ConnectionInput {
        id: Some("openai".into()),
        create: true,
        kind: "llm".into(),
        adapter: "openai".into(),
        endpoint: "https://api.openai.com/v1".into(),
        options: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let updated = runtime
        .call({
            let connection = connection.clone();
            move |app| {
                settings::update_settings(
                    app.world_mut(),
                    w::UpdateSettingsRequest {
                        revision: 1,
                        connections: w::ConnectionInputs {
                            items: vec![connection],
                            ..Default::default()
                        }
                        .into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(updated.0.revision, 2);
    let grant = w::CreateGrantRequest {
        request_key: "grant".into(),
        companion_id: companion.clone(),
        connection_id: "openai".into(),
        destinations: vec!["https://api.openai.com".into()],
        data_kinds: vec!["conversation".into()],
        purpose: "conversation".into(),
        consent: w::Consent {
            settings_revision: 2,
            provider_revision: "builtin-openai-v1".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let first = runtime
        .call({
            let grant = grant.clone();
            move |app| settings::create_grant(app.world_mut(), grant)
        })
        .await
        .unwrap();
    let replay = runtime
        .call({
            let grant = grant.clone();
            move |app| settings::create_grant(app.world_mut(), grant)
        })
        .await
        .unwrap();
    assert_eq!(first.id, replay.id);
    let mut changed = grant;
    changed.purpose = "task".into();
    changed.data_kinds = vec!["task".into()];
    assert!(
        runtime
            .call({
                let changed = changed.clone();
                move |app| settings::create_grant(app.world_mut(), changed)
            })
            .await
            .is_err()
    );
    let availability = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                settings::availability(&app.world().resource::<Host>().db, &companion, "openai")
            }
        })
        .await
        .unwrap();
    assert!(
        availability
            .operations
            .iter()
            .any(|operation| operation.operation == "conversation" && operation.runnable)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_oversized_reply_is_not_committed() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([vec![
        E::text("x".repeat(128_001)),
        E::final_response_with_default_usage(),
    ]]);
    let runtime = Runtime::start_with(temp.path(), model_options(model))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "huge", "too much").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seen = history(&runtime, target(&companion)).await;
    assert!(
        !seen
            .items
            .iter()
            .any(|message| message.text.len() > 128_000)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn streamed_deltas_and_turn_events_are_emitted_exactly_once_per_turn() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([vec![
        E::text("こん"),
        E::text("にちは"),
        E::final_response_with_default_usage(),
    ]]);
    let runtime = Runtime::start_with(temp.path(), model_options(model))
        .await
        .unwrap();
    let (companion, mut events) = runtime
        .call(|app| {
            let companion = mind::companions(&app.world().resource::<Host>().db)?
                .into_iter()
                .next()
                .unwrap()
                .id;
            let (_, receiver, _) = super::subscribe(app, None)?;
            Ok((companion, receiver))
        })
        .await
        .unwrap();
    send(&runtime, &companion, "stream", "hello").await;
    until(&runtime, &companion, "こんにちは").await;

    let mut deltas = Vec::new();
    let mut turn_states = Vec::new();
    while let Ok(notification) = events.try_recv() {
        match notification.event {
            Some(w::notification::Event::MessageDelta(delta)) => {
                deltas.push((delta.offset, delta.delta));
            }
            Some(w::notification::Event::TurnEvent(turn)) => {
                turn_states.push(turn.state);
            }
            _ => {}
        }
    }
    assert_eq!(deltas, vec![(0, "こん".into()), (2, "にちは".into())]);
    assert_eq!(turn_states, vec!["streaming", "answered"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn consolidation_runs_after_restart_without_new_chat_and_mind_state_is_not_duplicated() {
    let temp = tempfile::tempdir().unwrap();
    let first_model = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "mem",
                "mind_update",
                serde_json::json!({
                    "expected_revision": 1,
                    "state": {
                        "tendencies": ["穏やか"]
                    }
                }),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("了解"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(first_model.clone(), first_model, false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "turn-1", "緑茶が好き").await;
    until(&runtime, &companion, "了解").await;
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let second_model = MockCompletionModel::from_stream_turns([
        vec![
            E::text(
                "{\"reason\":\"再起動後の整理\",\"memories\":[{\"content\":\"緑茶が好き\",\"description\":\"好み\",\"appraisal\":\"落ち着いた\",\"acquisition\":\"user_statement\"}]}",
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("二度目"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(second_model.clone(), second_model, true),
    )
    .await
    .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime
                .call({
                    let companion = companion.clone();
                    move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                })
                .await
                .unwrap();
            if view
                .index
                .as_option()
                .is_some_and(|index| index.memories > 0)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consolidation did not run after restart");
    assert_eq!(view.index.as_option().unwrap().memories, 1);

    send(&runtime, &companion, "turn-2", "こんにちは").await;
    until(&runtime, &companion, "二度目").await;
    let seen = context_history(&runtime, target(&companion)).await;
    let mind_notes = seen
        .items
        .iter()
        .filter(|message| message.text.contains("現在の心（revision 2）:"))
        .count();
    assert_eq!(
        mind_notes, 1,
        "mind state note must not be duplicated across restart"
    );
    runtime.shutdown().await.unwrap();
}

fn memory(owner: &str, content: &str) -> mind::MemoryRecord {
    mind::MemoryRecord {
        id: "m1".into(),
        owner: owner.into(),
        kind: "experience".into(),
        audience: mind::PRIVATE.into(),
        content: content.into(),
        description: String::new(),
        appraisal: String::new(),
        acquisition: "user_statement".into(),
        sources: Vec::new(),
        recorded_at: "2026-11-10T00:00:00Z".into(),
        occurred_at: None,
        supersedes: Vec::new(),
        recall_preference: mind::NORMAL.into(),
        created_by: "parent".into(),
        revision: 1,
        updated_at: "2026-11-10T00:00:00Z".into(),
    }
}

async fn control(
    runtime: &Runtime,
    companion: &str,
    action: &str,
) -> super::db::Result<w::MemoryControlResult> {
    let (companion, action) = (companion.to_owned(), action.to_owned());
    tokio::time::timeout(
        Duration::from_secs(3),
        runtime.call(move |app| {
            mind::control_memory(
                app.world_mut(),
                w::MemoryControlRequest {
                    companion_id: companion,
                    memory_id: "m1".into(),
                    action,
                    ..Default::default()
                },
            )
        }),
    )
    .await
    .expect("control_memory must return")
}

#[tokio::test]
async fn memory_control_forgets_shares_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    let owner = companion.clone();
    runtime
        .call(move |app| {
            app.world()
                .resource::<Host>()
                .db
                .transaction(|tx| tx.put_memory(&memory(&owner, "りんごが好き")))
        })
        .await
        .unwrap();

    let forgotten = control(&runtime, &companion, "forget").await.unwrap();
    assert_eq!(forgotten.recall_preference, mind::FORGOTTEN);
    assert_eq!(forgotten.revision, 2);
    let replay = control(&runtime, &companion, "forget").await.unwrap();
    assert_eq!(replay.revision, 2, "a repeated action must not re-revision");

    let shared = control(&runtime, &companion, "share").await.unwrap();
    assert_eq!(shared.audience, mind::SHARED);
    let private = control(&runtime, &companion, "unshare").await.unwrap();
    assert_eq!(private.audience, mind::PRIVATE);

    let record = runtime
        .call({
            let companion = companion.clone();
            move |app| mind::companion(&app.world().resource::<Host>().db, &companion)
        })
        .await
        .unwrap();
    assert_eq!(record.memory_revision, 4);

    let ghost = runtime
        .call(move |app| {
            mind::control_memory(
                app.world_mut(),
                w::MemoryControlRequest {
                    companion_id: "ghost".into(),
                    memory_id: "m1".into(),
                    action: "forget".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert_eq!(ghost.unwrap_err().code, connectrpc::ErrorCode::NotFound);
    runtime.shutdown().await.unwrap();
}

async fn context_history(runtime: &Runtime, target: w::Target) -> w::History {
    runtime
        .call(move |app| {
            chat::history(
                app.world_mut(),
                w::HistoryRequest {
                    target: target.into(),
                    include_context: Some(true),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap()
}

fn consolidation(reply: &str) -> Vec<rig_core::test_utils::MockStreamEvent> {
    vec![
        E::text(reply.to_owned()),
        E::final_response_with_default_usage(),
    ]
}

#[tokio::test]
async fn consolidation_does_not_relearn_injected_context() {
    let temp = tempfile::tempdir().unwrap();
    let empty = "{\"reason\":\"なし\",\"memories\":[]}";
    let model = MockCompletionModel::from_stream_turns([
        vec![E::text("はい"), E::final_response_with_default_usage()],
        consolidation(empty),
        vec![E::text("そうだね"), E::final_response_with_default_usage()],
        consolidation(empty),
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(model.clone(), model.clone(), true))
        .await
        .unwrap();
    let companion = runtime
        .call(|app| {
            let host = app.world_mut().resource_mut::<Host>();
            let companion = mind::companions(&host.db)?.into_iter().next().unwrap().id;
            host.db
                .transaction(|tx| tx.put_memory(&memory(&companion, "りんごが好き")))?;
            let mut record = mind::companion(&host.db, &companion)?;
            record.state.concerns = vec!["静かな午後".into()];
            host.db.put(&format!("companion:{companion}"), &record)?;
            Ok(companion)
        })
        .await
        .unwrap();
    for (key, text, answer) in [
        ("one", "りんごが好き", "はい"),
        ("two", "りんごが好きだよね", "そうだね"),
    ] {
        send(&runtime, &companion, key, text).await;
        until(&runtime, &companion, answer).await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.requests().len() < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consolidation did not run");

    let prompts: Vec<String> = model
        .requests()
        .iter()
        .map(|request| format!("{:?}", request.chat_history))
        .collect();
    let batches: Vec<&String> = prompts
        .iter()
        .filter(|prompt| prompt.contains("以下は保存済みの会話"))
        .collect();
    assert_eq!(batches.len(), 2, "both turns must be consolidated once");
    let last = batches.last().unwrap();
    assert!(
        !last.contains("関連する記憶"),
        "recall notes must not be re-learned: {last}"
    );
    assert!(
        !last.contains("現在の心（revision"),
        "mind state notes must not be re-learned: {last}"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_a_turns_own_call_streams_into_the_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("こんにちは"))
        .await
        .unwrap();
    let (companion, mut events) = runtime
        .call(|app| {
            let companion = mind::companions(&app.world().resource::<Host>().db)?
                .into_iter()
                .next()
                .unwrap()
                .id;
            let (_, receiver, _) = super::subscribe(app, None)?;
            Ok((companion, receiver))
        })
        .await
        .unwrap();
    send(&runtime, &companion, "stream", "hi").await;
    until(&runtime, &companion, "こんにちは").await;
    while events.try_recv().is_ok() {}

    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                let world = app.world_mut();
                let parent = chat::find_agent(world, &chat::parent_id(&companion))
                    .ok_or_else(|| ConnectError::unavailable("parent agent is missing"))?;
                // A settled turn is despawned, so stand one in: the kernel
                // ignores a turn without `TurnOf`, but streams still read it.
                let turn = world.spawn(Turn::default()).id();
                let side = world.spawn((Effect::default(), CallOf(turn))).id();
                let own = world
                    .spawn((Effect::default(), CallOf(turn), TurnCall))
                    .id();
                for (effect, text) in [(side, "side call"), (own, "turn call")] {
                    world.write_message(StreamDelta {
                        effect,
                        agent: Some(parent),
                        event: StreamEvent::Text {
                            part: Part::new(0),
                            text: text.into(),
                        },
                    });
                }
                Ok(())
            }
        })
        .await
        .unwrap();

    let mut deltas = Vec::new();
    while let Ok(notification) = events.try_recv() {
        if let Some(w::notification::Event::MessageDelta(delta)) = notification.event {
            deltas.push(delta.delta);
        }
    }
    assert_eq!(
        deltas,
        vec!["turn call".to_owned()],
        "a side call must not stream into the conversation"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn consolidation_keeps_the_acquisition_and_drops_unknown_ones() {
    for (output, stored) in [
        (
            "{\"reason\":\"報告\",\"memories\":[{\"content\":\"紅茶が好き\",\"acquisition\":\"report\"}]}",
            1,
        ),
        (
            "```json\n{\"reason\":\"報告\",\"memories\":[{\"content\":\"紅茶が好き\",\"acquisition\":\"report\"}]}\n```",
            1,
        ),
        (
            "{\"reason\":\"不明\",\"memories\":[{\"content\":\"紅茶が好き\",\"acquisition\":\"made_up\"}]}",
            0,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let model = MockCompletionModel::from_stream_turns([
            vec![E::text("first"), E::final_response_with_default_usage()],
            consolidation(output),
        ]);
        let runtime = Runtime::start_with(temp.path(), scripted(model.clone(), model, true))
            .await
            .unwrap();
        let companion = default_companion(&runtime).await;
        send(&runtime, &companion, "memory", "紅茶が好き").await;
        until(&runtime, &companion, "first").await;
        let view = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let view = runtime
                    .call({
                        let companion = companion.clone();
                        move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                    })
                    .await
                    .unwrap();
                if view
                    .consolidation
                    .as_option()
                    .is_some_and(|c| c.state == "ready")
                {
                    return view;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("consolidation did not become ready");
        assert_eq!(view.index.as_option().unwrap().memories, stored);
        if stored == 1 {
            let found = runtime
                .call({
                    let companion = companion.clone();
                    move |app| {
                        app.world().resource::<Host>().db.search_memories(
                            &companion,
                            "紅茶が好き",
                            false,
                            1,
                        )
                    }
                })
                .await
                .unwrap();
            assert_eq!(found[0].acquisition, "report");
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn an_unknown_target_never_spawns_an_agent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    runtime
        .call(|app| chat::stop(app, target("ghost")))
        .await
        .unwrap();
    let message = runtime
        .call(move |app| {
            chat::message(
                app.world_mut(),
                w::MessageRequest {
                    target: target("ghost").into(),
                    message_id: "1".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert_eq!(message.unwrap_err().code, connectrpc::ErrorCode::NotFound);
    let accept = runtime
        .call(move |app| {
            chat::accept(
                app,
                w::SendMessageRequest {
                    target: target("ghost").into(),
                    request_key: "ghost".into(),
                    text: "hi".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert_eq!(accept.unwrap_err().code, connectrpc::ErrorCode::NotFound);
    let agents = runtime
        .call(|app| {
            Ok(app
                .world_mut()
                .query::<&AgentId>()
                .iter(app.world())
                .map(|agent| agent.0.clone())
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert!(
        !agents.iter().any(|agent| agent == "parent-ghost"),
        "an unknown companion must not get an agent: {agents:?}"
    );
    runtime.shutdown().await.unwrap();
}

async fn elapsed(runtime: &Runtime) -> Duration {
    runtime
        .read(|app| Ok(app.world().resource::<Time<Virtual>>().elapsed()))
        .await
        .unwrap()
}

#[tokio::test]
async fn reads_do_not_advance_the_simulation_time() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    let before = elapsed(&runtime).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    for _ in 0..3 {
        runtime
            .read(|app| super::snapshot(app, None))
            .await
            .unwrap();
    }
    let after = elapsed(&runtime).await;
    assert!(
        after.saturating_sub(before) < Duration::from_millis(20),
        "a read must not advance the simulation: {before:?} -> {after:?}"
    );
    runtime.call(|_| Ok(())).await.unwrap();
    let ticked = elapsed(&runtime).await;
    assert!(
        ticked.saturating_sub(after) >= Duration::from_millis(20),
        "a write must advance it: {after:?} -> {ticked:?}"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn wakes_never_consume_the_command_queue() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    runtime
        .call(|app| {
            let wake = app.world().resource::<Wake>();
            for _ in 0..100_000 {
                wake.wake();
            }
            Ok(())
        })
        .await
        .unwrap();
    for _ in 0..3 {
        runtime
            .read(|app| super::snapshot(app, None))
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .expect("a stopped host must return")
        .unwrap();
}

#[tokio::test]
async fn consolidation_runs_for_the_parent_while_a_work_agent_exists() {
    let temp = tempfile::tempdir().unwrap();
    let parent = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "start",
                "start_work",
                serde_json::json!({"prompt": "整理して"}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("started"), E::final_response_with_default_usage()],
        // A work report asks the parent for a turn, and that turn and the
        // consolidation call race for the next entry: both take the same
        // reply so either order reads as a consolidation.
        consolidation("{\"reason\":\"なし\",\"memories\":[]}"),
        consolidation("{\"reason\":\"なし\",\"memories\":[]}"),
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("done"), true))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "work", "任せる").await;
    until(&runtime, &companion, "started").await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime
                .read({
                    let companion = companion.clone();
                    move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                })
                .await
                .unwrap();
            if view
                .consolidation
                .as_option()
                .is_some_and(|cursor| cursor.last_sequence > 0)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the parent must still be consolidated with a work agent present");
    assert!(
        view.consolidation
            .as_option()
            .is_some_and(|cursor| cursor.state == "ready")
    );
    runtime.shutdown().await.unwrap();
}

fn starts_with_tool_result(messages: &[Arc<rig_core::completion::Message>]) -> bool {
    messages
        .iter()
        .find(|message| !matches!(&***message, rig_core::completion::Message::System { .. }))
        .is_some_and(|message| match &**message {
            rig_core::completion::Message::User { content } => content
                .iter()
                .all(|item| matches!(item, UserContent::ToolResult(_))),
            _ => false,
        })
}

#[tokio::test]
async fn the_window_bounds_what_the_model_reads_and_keeps_history_whole() {
    let temp = tempfile::tempdir().unwrap();
    let mut turns = vec![
        vec![
            E::tool_call("clock", "current_time", serde_json::json!({})),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("ok"), E::final_response_with_default_usage()],
    ];
    for n in 0..4 {
        turns.push(vec![
            E::text(format!("answer {n}")),
            E::final_response_with_default_usage(),
        ]);
    }
    let model = MockCompletionModel::from_stream_turns(turns);
    let runtime = Runtime::start_with(
        temp.path(),
        windowed(model.clone(), Some(600), Some(100), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    for key in ["one", "two", "three", "four"] {
        send(&runtime, &companion, key, &"x".repeat(600)).await;
    }
    until(&runtime, &companion, "answer 2").await;

    let sent = model.requests().last().cloned().expect("a model call");
    assert_eq!(sent.max_tokens, Some(100), "the output limit must be set");
    let seen = history(&runtime, target(&companion)).await;
    assert!(
        seen.items.len() >= 4,
        "GetHistory must keep every message: {}",
        seen.items.len()
    );
    assert!(
        sent.chat_history.len() <= seen.items.len(),
        "the window must cut what the model reads: {} of {}",
        sent.chat_history.len(),
        seen.items.len()
    );
    assert!(
        !starts_with_tool_result(&sent.chat_history),
        "a tool result must not lose its call"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_context_window_is_reported_and_left_whole() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), windowed(text_model("ok"), None, None, false))
        .await
        .unwrap();
    let (companion, mut events) = runtime
        .call(|app| {
            let companion = mind::companions(&app.world().resource::<Host>().db)?
                .into_iter()
                .next()
                .unwrap()
                .id;
            let (_, receiver, _) = super::subscribe(app, None)?;
            Ok((companion, receiver))
        })
        .await
        .unwrap();
    send(&runtime, &companion, "unknown", &"x".repeat(64)).await;
    until(&runtime, &companion, "ok").await;
    let code = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while let Ok(notification) = events.try_recv() {
                if let Some(w::notification::Event::Notice(notice)) = notification.event {
                    return notice.code;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an unknown window must be reported");
    assert_eq!(code, "context_window_unknown");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_broken_consolidation_reply_is_reported_and_holds_the_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![E::text("first"), E::final_response_with_default_usage()],
        consolidation("this is not json"),
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(model.clone(), model, true))
        .await
        .unwrap();
    let (companion, mut events) = runtime
        .call(|app| {
            let companion = mind::companions(&app.world().resource::<Host>().db)?
                .into_iter()
                .next()
                .unwrap()
                .id;
            let (_, receiver, _) = super::subscribe(app, None)?;
            Ok((companion, receiver))
        })
        .await
        .unwrap();
    send(&runtime, &companion, "broken", "紅茶が好き").await;
    until(&runtime, &companion, "first").await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime
                .read({
                    let companion = companion.clone();
                    move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                })
                .await
                .unwrap();
            if view
                .consolidation
                .as_option()
                .is_some_and(|c| c.state == "error")
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a broken reply must be reported");
    let cursor = view.consolidation.as_option().unwrap();
    assert_eq!(cursor.last_sequence, 0, "the cursor must not advance");
    let code = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while let Ok(notification) = events.try_recv() {
                if let Some(w::notification::Event::Notice(notice)) = notification.event {
                    return notice.code;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a failed consolidation must be reported");
    assert_eq!(code, "consolidation_failed");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_batch_that_never_parses_is_skipped_instead_of_stopping_memory() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![E::text("返事"), E::final_response_with_default_usage()],
        consolidation("this is not json"),
        consolidation("this is not json"),
        consolidation("this is not json"),
        consolidation("this is not json"),
        consolidation("this is not json"),
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(model.clone(), model, true))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "bad", "紅茶が好き").await;
    until(&runtime, &companion, "返事").await;
    let cursor = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // No further turn ends, so the queue is nudged to retry.
            runtime
                .call({
                    let companion = companion.clone();
                    move |app| {
                        app.world_mut()
                            .resource_mut::<Host>()
                            .consolidation_queue
                            .insert(companion);
                        Ok(())
                    }
                })
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            let view = runtime
                .read({
                    let companion = companion.clone();
                    move |app| mind::mind_view(&app.world().resource::<Host>().db, &companion)
                })
                .await
                .unwrap();
            let cursor = view.consolidation.as_option().cloned().unwrap();
            if cursor.skipped == 1 {
                return cursor;
            }
        }
    })
    .await
    .expect("a batch the model never answers must be skipped, not retried forever");
    assert!(
        cursor.last_sequence > 0,
        "skipping must move the cursor past the bad batch"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_authors_profile_is_a_prompt_section_not_a_per_call_edit() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![E::text("はい"), E::final_response_with_default_usage()],
        vec![E::text("そうだね"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), model_options(model.clone()))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                mind::update_mind_profile(
                    app.world_mut(),
                    w::UpdateMindProfileRequest {
                        companion_id: companion,
                        expected_revision: 1,
                        facets: vec!["静かで穏やか".into()],
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    send(&runtime, &companion, "one", "こんにちは").await;
    until(&runtime, &companion, "はい").await;
    send(&runtime, &companion, "two", "元気？").await;
    until(&runtime, &companion, "そうだね").await;

    let sent = model.requests();
    assert!(sent.len() >= 2);
    for request in &sent {
        let system = match request.chat_history.first().map(|message| &**message) {
            Some(rig_core::completion::Message::System { content }) => content.clone(),
            other => panic!("a request must open with its system prompt: {other:?}"),
        };
        assert!(
            system.contains("静かで穏やか"),
            "the profile must be in the prompt: {system}"
        );
        assert!(
            system.contains("<facets>"),
            "the profile must be a tagged section: {system}"
        );
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_work_report_asks_the_parent_for_a_turn_of_its_own() {
    let temp = tempfile::tempdir().unwrap();
    let parent = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "start",
                "start_work",
                serde_json::json!({"prompt": "調べて"}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("started"), E::final_response_with_default_usage()],
        vec![
            E::text("報告を受けた"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(parent, text_model("done"), false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "work", "任せる").await;
    until(&runtime, &companion, "started").await;
    // No user message: the report itself starts the parent's next turn.
    let seen = until(&runtime, &companion, "報告を受けた").await;
    let reports = seen
        .items
        .iter()
        .filter(|message| message.text.contains("done") && message.role == "agent")
        .count();
    assert_eq!(reports, 1, "a finished work is reported exactly once");
    let record = runtime
        .read(|app| {
            let records = app
                .world()
                .resource::<Host>()
                .db
                .list::<WorkRecord>("work:")?;
            Ok(records.into_iter().next().unwrap().1)
        })
        .await
        .unwrap();
    assert!(
        !record.reported_run.is_empty(),
        "the reported run must be recorded"
    );
    runtime.shutdown().await.unwrap();
}

/// A turn a restart starts, with no request to bind the model at.
#[tokio::test]
async fn a_report_starts_the_parents_turn_after_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("はじめまして"))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "hello", "やあ").await;
    until(&runtime, &companion, "はじめまして").await;
    runtime.shutdown().await.unwrap();
    drop(runtime);

    // No `SendMessage` follows: the report is the only way into the
    // conversation, as a work report or a scheduled wake would be.
    let runtime = Runtime::start_with(temp.path(), factory("報告を読んだ"))
        .await
        .unwrap();
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::deliver_to_parent(
                    app.world_mut(),
                    &companion,
                    "work-1",
                    "作業が終わった",
                    None,
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let seen = until(&runtime, &companion, "報告を読んだ").await;
    assert!(
        seen.items
            .iter()
            .any(|message| message.text == "報告を読んだ"),
        "the parent's own turn must answer a host-originated input"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_empty_mind_still_states_its_revision() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("はい"))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "empty", "こんにちは").await;
    until(&runtime, &companion, "はい").await;
    let seen = context_history(&runtime, target(&companion)).await;
    let notes = seen
        .items
        .iter()
        .filter(|message| {
            message
                .text
                .contains("現在の心（revision 1）: （まだ何もありません）")
        })
        .count();
    assert_eq!(notes, 1, "the empty mind is stated, once");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stale_mind_update_names_the_current_revision() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            E::tool_call(
                "mem",
                "mind_update",
                serde_json::json!({
                    "expected_revision": 9,
                    "memories": [{"content": "りんごが好き", "acquisition": "user_statement"}]
                }),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("はい"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), model_options(model.clone()))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "stale", "りんごが好き").await;
    until(&runtime, &companion, "はい").await;
    let refusal = model
        .requests()
        .iter()
        .flat_map(|request| request.chat_history.iter())
        .map(|message| format!("{message:?}"))
        .find(|text| text.contains("mind revision changed"))
        .expect("the refusal must reach the model");
    assert!(
        refusal.contains("current revision is 1"),
        "the refusal must name the current revision: {refusal}"
    );
    runtime.shutdown().await.unwrap();
}

async fn update_settings(runtime: &Runtime, items: Vec<w::ConnectionInput>) -> Result<w::Settings> {
    runtime
        .call(move |app| {
            settings::update_settings(
                app.world_mut(),
                w::UpdateSettingsRequest {
                    revision: 1,
                    connections: w::ConnectionInputs {
                        items,
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                },
            )
            .map(|(settings, _)| settings)
        })
        .await
}

fn connection_input(adapter: &str, kind: &str) -> w::ConnectionInput {
    w::ConnectionInput {
        id: Some("openai".into()),
        create: true,
        kind: kind.into(),
        adapter: adapter.into(),
        endpoint: "https://api.openai.com/v1".into(),
        options: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_connection_must_name_its_adapter_and_kind() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), Options::default())
        .await
        .unwrap();
    for (adapter, kind) in [("", "llm"), ("openai", ""), ("openai", "embedding")] {
        let refused = update_settings(&runtime, vec![connection_input(adapter, kind)]).await;
        let error = refused.expect_err("an unsupported connection must be refused");
        assert_eq!(
            error.code,
            connectrpc::ErrorCode::InvalidArgument,
            "{adapter}/{kind} was refused with {error:?}"
        );
    }
    update_settings(&runtime, vec![connection_input("openai", "llm")])
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_grant_must_cover_the_data_its_purpose_sends() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), Options::default())
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    update_settings(&runtime, vec![connection_input("openai", "llm")])
        .await
        .unwrap();
    let grant = w::CreateGrantRequest {
        request_key: "grant".into(),
        companion_id: companion,
        connection_id: "openai".into(),
        destinations: vec!["https://api.openai.com".into()],
        data_kinds: vec!["task".into()],
        purpose: "conversation".into(),
        consent: w::Consent {
            settings_revision: 2,
            provider_revision: "builtin-openai-v1".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let refused = runtime
        .call(move |app| settings::create_grant(app.world_mut(), grant))
        .await;
    assert_eq!(
        refused.unwrap_err().code,
        connectrpc::ErrorCode::InvalidArgument,
        "a grant must cover the kinds its purpose sends"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unsupported_backend_is_not_run() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                app.world().resource::<Host>().db.put(
                    "work:acp",
                    &WorkRecord {
                        session: w::WorkSession {
                            id: "acp".into(),
                            companion_id: companion,
                            backend: "acp".into(),
                            status: "running".into(),
                            ..Default::default()
                        },
                        reported_run: String::new(),
                    },
                )
            }
        })
        .await
        .unwrap();
    let refused = runtime.call(|app| work::view(app, "acp")).await;
    assert_eq!(
        refused.unwrap_err().code,
        connectrpc::ErrorCode::Unavailable,
        "a backend the host cannot run is unavailable, not empty"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_advertised_companion_limit_is_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), factory("ok"))
        .await
        .unwrap();
    for index in 1..mind::MAX_COMPANIONS {
        runtime
            .call(move |app| {
                mind::create_companion(
                    app.world_mut(),
                    w::CreateCompanionRequest {
                        name: format!("companion {index}"),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
    }
    let refused = runtime
        .call(|app| {
            mind::create_companion(
                app.world_mut(),
                w::CreateCompanionRequest {
                    name: "too many".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert_eq!(
        refused.unwrap_err().code,
        connectrpc::ErrorCode::ResourceExhausted
    );
    let count = runtime
        .read(|app| Ok(mind::companions(&app.world().resource::<Host>().db)?.len()))
        .await
        .unwrap();
    assert_eq!(count, mind::MAX_COMPANIONS);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_removed_connection_takes_its_credential_and_grants_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(temp.path(), Options::default())
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    update_settings(&runtime, vec![connection_input("openai", "llm")])
        .await
        .unwrap();
    runtime
        .call(|app| {
            let db = &app.world().resource::<Host>().db;
            let key = "connection:openai";
            let mut record: settings::ConnectionRecord = db.get(key)?.unwrap();
            record.credential_ref = Some("connection:openai".into());
            record.connection.credential_state = "registered".into();
            db.put(key, &record)
        })
        .await
        .unwrap();
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                settings::create_grant(
                    app.world_mut(),
                    w::CreateGrantRequest {
                        request_key: "grant".into(),
                        companion_id: companion,
                        connection_id: "openai".into(),
                        destinations: vec!["https://api.openai.com".into()],
                        data_kinds: vec!["conversation".into()],
                        purpose: "conversation".into(),
                        consent: w::Consent {
                            settings_revision: 2,
                            provider_revision: "builtin-openai-v1".into(),
                            ..Default::default()
                        }
                        .into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();

    let (updated, forgotten) = runtime
        .call(|app| {
            settings::update_settings(
                app.world_mut(),
                w::UpdateSettingsRequest {
                    revision: 2,
                    connections: w::ConnectionInputs::default().into(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(updated.connections.is_empty());
    assert_eq!(forgotten, vec!["connection:openai".to_owned()]);
    let grants = runtime
        .read(|app| settings::grants(&app.world().resource::<Host>().db, None))
        .await
        .unwrap();
    assert_eq!(grants[0].status, "revoked");
    runtime.shutdown().await.unwrap();
}

async fn until_assistant(runtime: &Runtime, companion: &str, count: usize) -> w::History {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let seen = history(runtime, target(companion)).await;
            let answered = seen
                .items
                .iter()
                .filter(|message| message.role == "assistant")
                .count();
            if answered >= count {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn did not finish")
}

#[tokio::test]
async fn the_window_keeps_a_summary_of_what_it_cut_without_touching_history() {
    let temp = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns(
        (0..10).map(|_| vec![E::text("ok"), E::final_response_with_default_usage()]),
    );
    let runtime = Runtime::start_with(
        temp.path(),
        windowed(model.clone(), Some(600), Some(100), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    // Everything so far counts as consolidated, so the window may summarize.
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                app.world().resource::<Host>().db.put(
                    &format!("memory-cursor:{companion}"),
                    &mind::Consolidation {
                        last_sequence: u64::MAX,
                        state: "ready".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    for (index, key) in ["one", "two", "three", "four"].iter().enumerate() {
        send(&runtime, &companion, key, &"x".repeat(600)).await;
        until_assistant(&runtime, &companion, index + 1).await;
    }
    let summary = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stored = runtime
                .read({
                    let companion = companion.clone();
                    move |app| {
                        let agent = chat::find_agent(app.world_mut(), &chat::parent_id(&companion));
                        Ok(
                            agent
                                .and_then(|agent| app.world().get::<mind::Summary>(agent).cloned()),
                        )
                    }
                })
                .await
                .unwrap();
            if stored
                .as_ref()
                .is_some_and(|summary| !summary.text.is_empty())
            {
                return stored.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a cut window must be summarized");
    assert_eq!(summary.text, "ok");

    send(&runtime, &companion, "five", &"x".repeat(600)).await;
    until_assistant(&runtime, &companion, 5).await;
    let prompts: Vec<String> = model
        .requests()
        .iter()
        .map(|request| format!("{:?}", request.chat_history))
        .collect();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("<conversation>")),
        "the window must ask for a summary"
    );
    assert!(
        prompts.iter().any(|prompt| prompt.contains("<summary>")),
        "the summary must reach a later request"
    );
    let seen = history(&runtime, target(&companion)).await;
    assert!(
        !seen
            .items
            .iter()
            .any(|message| message.text.contains("<summary>")),
        "the summary is the request's own, not the conversation's"
    );
    runtime.shutdown().await.unwrap();
}
