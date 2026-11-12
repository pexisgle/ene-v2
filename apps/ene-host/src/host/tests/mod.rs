//! Behaviour tests for the headless host. The model is scripted; no external
//! service is called.

use super::{
    Options, Runtime, Store,
    agent::AgentKey,
    chat, client, compaction,
    db::{new_id, now},
    error::HostError,
    mind::{self, Companion, Memory},
    model::{Binding, ModelSource, Purpose},
    permission, resources, settings,
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
        acquisition: mind::Acquisition::UserStatement,
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

mod approval;
mod chat_tests;
mod client_tests;
mod compaction_tests;
mod mind_tests;
mod store;
mod work_tests;
