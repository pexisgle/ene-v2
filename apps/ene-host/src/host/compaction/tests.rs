mod budget;
mod persistence;
mod runtime;

use super::budget::cut;
use super::*;
use crate::host::mind::{Companion, ModelAssignment, ModelAssignments, Profile, State};
use ene_mind::JsonDoc;
use rig_core::catalog::{Catalog, Connector, ModelSpec};
use rig_core::completion::{AssistantContent, AssistantMessage};
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::conversation::TurnEnd;
use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
use rig_core::providers::registry::ProviderId;
use rig_core::serve::{ErasedHandler, adapters::ModelAdapter};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use std::sync::atomic::{AtomicBool, Ordering};

fn tool_turn(history: &mut History, call: &str, output: String) {
    let id = CallId::from_wire(call);
    let name = ToolName::new("read").unwrap();
    let call = ToolCall::new(
        id.clone(),
        ToolFunction::new(name.clone(), serde_json::json!({ "path": "a.rs" })),
    );
    history.extend([
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
            call,
        )])),
        Message::tool_result(id, name, output),
    ]);
}

fn conversation(turns: usize) -> History {
    let mut history = History::new();
    for turn in 0..turns {
        history.push(Message::user(format!("question {turn} {}", "x".repeat(40))));
        tool_turn(&mut history, &format!("call-{turn}"), "z".repeat(400));
        history.push(Message::assistant(format!(
            "answer {turn} {}",
            "y".repeat(40)
        )));
    }
    history
}

fn tool(description: &str) -> Arc<ToolDefinition> {
    Arc::new(ToolDefinition::new(
        ToolName::new("read").unwrap(),
        description,
        serde_json::json!({ "type": "object", "properties": {} }),
    ))
}

fn empty_request() -> CompletionRequest {
    let mut request = CompletionRequest::new("");
    request.chat_history.clear();
    request
}

fn noticed(history: &History) -> bool {
    history.entries().next().is_some_and(
        |entry| matches!(entry.message, Message::System { content } if content == NOTICE),
    )
}

fn head_text(history: &History) -> String {
    let entry = history.entries().next().expect("an injected head");
    match entry.message {
        Message::System { content } => content.clone(),
        _ => panic!("the injected head is not a system message"),
    }
}

/// Sets `dropped` when the task it guards is dropped.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn background_handles_run_on_tokio_wake_and_abort_on_drop() {
    use std::sync::atomic::AtomicUsize;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake = Wake::new({
        let wakes = wakes.clone();
        move || {
            wakes.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (done, received) = tokio::sync::oneshot::channel();
    let mut task = background::spawn(runtime.handle(), Some(&wake), async move {
        let value = tokio::time::timeout(Duration::from_secs(1), async { 42 })
            .await
            .unwrap();
        done.send(()).unwrap();
        value
    });
    runtime.block_on(received).unwrap();
    assert_eq!(task.poll(), std::task::Poll::Ready(Some(42)));
    assert_eq!(wakes.load(Ordering::Relaxed), 1);

    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let task = background::spawn(runtime.handle(), Some(&wake), async move {
        let _guard = guard;
        std::future::pending::<()>().await
    });
    drop(task);
    runtime.block_on(tokio::task::yield_now());
    assert!(dropped.load(Ordering::Relaxed));
    assert_eq!(
        wakes.load(Ordering::Relaxed),
        1,
        "cancellation does not report completion"
    );
}
