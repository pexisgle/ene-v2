//! The parent's Rig execution kernel.
//!
//! One headless Rig app owns the parent agents' inboxes, turns, model calls,
//! tool calls and session logs. This module only carries commands into that
//! app and reports what it did; it does not run a second turn loop, keep a
//! second conversation or decide when to call a model.
//!
//! The kernel runs on its own thread and sleeps between wakes. Every model
//! and tool call still runs on the kernel's task pools; the thread only draws
//! frames when something moved, as the bundled Rig host does.

mod kernel;
mod messages;
pub(crate) mod mind;

use crate::domain::{Error, ErrorKind};
use ene_rpc as w;
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

/// How long a query may wait for a frame of the kernel.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a shutdown may wait for the kernel to save and exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What the parent's execution did, in the order it happened. The host turns
/// these into notifications; the session log, not these events, is the saved
/// state.
#[derive(Debug)]
pub(crate) enum Event {
    /// A turn took the accepted inputs into its conversation and started.
    TurnStarted {
        companion: String,
        stream_id: String,
        request_id: Option<String>,
        messages: Vec<w::Message>,
    },
    /// A turn's reply streamed more text.
    Delta {
        companion: String,
        stream_id: String,
        offset: u64,
        delta: String,
    },
    /// A turn ended. `messages` are the entries this end made visible for the
    /// first time, such as the final reply or a stopped partial reply.
    TurnFinished {
        companion: String,
        stream_id: String,
        request_id: Option<String>,
        state: &'static str,
        message_id: Option<String>,
        messages: Vec<w::Message>,
        failure: Option<w::Failure>,
    },
    /// The session log stopped accepting writes. Nothing after it claims to be
    /// saved, and recovery is required.
    StorageFailed { reason: String },
    /// The kernel app itself stopped working. Running turns are not answered.
    Fatal { reason: String },
    /// A failed turn took the user's unanswered input out of the conversation
    /// and returned it to the client.
    Recalled {
        companion: String,
        text: String,
        why: Option<String>,
    },
}

/// The handle the host uses; the kernel's state is not reachable through it.
pub(crate) struct Runtime {
    /// `std::sync::mpsc::Sender` is not `Sync`; the lock also orders sends
    /// from concurrent requests without a second queue.
    signals: Mutex<Sender<Signal>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

enum Signal {
    Command(Command),
    /// A task finished or streamed something: draw a frame.
    Wake,
}

enum Command {
    Connect {
        companion: String,
        prompt: String,
        model: rig_ecs::model::Connection,
        mind_tools: bool,
    },
    /// Replaces the live model input after a committed mind or profile change.
    /// It does not stop a running turn or reconnect the model.
    SetMindPrompt {
        companion: String,
        prompt: String,
    },
    Deliver {
        companion: String,
        text: String,
        request_id: String,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    HasRequest {
        companion: String,
        request_id: String,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
    Stop {
        companion: String,
        reason: Option<String>,
    },
    History {
        companion: String,
        cursor: Option<String>,
        limit: usize,
        reply: oneshot::Sender<Result<w::History, Error>>,
    },
    HoldRecall {
        companion: String,
        request_id: String,
        deadline: std::time::Instant,
        reply: Option<oneshot::Sender<()>>,
    },
    RecallCompleted {
        request_id: String,
        prompt: String,
    },
    RecallCancelled {
        request_id: String,
    },
    HistoryAfter {
        companion: String,
        after: u64,
        limit: usize,
        reply: oneshot::Sender<Result<w::History, Error>>,
    },
    Message {
        companion: String,
        id: String,
        reply: oneshot::Sender<Result<w::Message, Error>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

impl Runtime {
    /// Opens the session directory and starts the kernel thread. The returned
    /// receiver carries what the kernel did; the host consumes it on its
    /// runtime, so it must be drained before the thread fills it.
    ///
    /// The closure runs on the kernel thread before the app finishes and
    /// registers app extensions the host owns, such as the host-bound mind
    /// tools.
    pub(crate) fn start_with(
        directory: std::path::PathBuf,
        configure: impl FnOnce(&mut rig_ecs::prelude::App) + Send + 'static,
    ) -> Result<(Self, tokio_mpsc::UnboundedReceiver<Event>), Error> {
        Self::start_inner(directory, Box::new(configure))
    }

    fn start_inner(
        directory: std::path::PathBuf,
        configure: kernel::Configure,
    ) -> Result<(Self, tokio_mpsc::UnboundedReceiver<Event>), Error> {
        let (events, received) = tokio_mpsc::unbounded_channel();
        // Open before reporting success: a session directory the store
        // refuses must fail the start, not the first message.
        let store = rig_memory::DirStore::open(&directory).map_err(|_| Error {
            kind: ErrorKind::StorageFailure,
            reason: Some("session_store_unavailable"),
        })?;
        let (sender, receiver) = mpsc::channel();
        let waker = sender.clone();
        let thread = std::thread::Builder::new()
            .name("ene-rig".into())
            // The app's schedules and the session log recurse deeper than a
            // data-only thread's default stack.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || kernel::run(store, receiver, waker, events, configure))
            .map_err(|_| Error::unavailable())?;
        Ok((
            Self {
                signals: Mutex::new(sender),
                thread: Mutex::new(Some(thread)),
            },
            received,
        ))
    }

    fn send(&self, signal: Signal) -> Result<(), Error> {
        let sender = self.signals.lock().map_err(|_| Error::unavailable())?;
        sender.send(signal).map_err(|_| Error::unavailable())
    }

    /// Makes `model` the companion's model and `prompt` its system prompt,
    /// stopping a turn that is still running so the next call uses the new
    /// configuration.
    pub(crate) fn connect(
        &self,
        companion: String,
        prompt: String,
        model: rig_ecs::model::Connection,
        mind_tools: bool,
    ) -> Result<(), Error> {
        self.send(Signal::Command(Command::Connect {
            companion,
            prompt,
            model,
            mind_tools,
        }))
    }

    /// Updates the live system prompt from a committed mind revision. The next
    /// model call in the running turn sees it; the turn is not stopped.
    pub(crate) fn set_mind_prompt(&self, companion: String, prompt: String) -> Result<(), Error> {
        self.send(Signal::Command(Command::SetMindPrompt {
            companion,
            prompt,
        }))
    }

    /// Adds the input to the companion's Rig inbox. Returns once the kernel
    /// accepted the message and wrote that acceptance to the session log, not
    /// once a model answered; a busy agent keeps it for the turn's next call
    /// (`Steer`).
    pub(crate) async fn deliver(
        &self,
        companion: String,
        text: String,
        request_id: String,
    ) -> Result<(), Error> {
        self.query(|reply| Command::Deliver {
            companion,
            text,
            request_id,
            reply,
        })
        .await
    }

    /// Sends one command and waits a query timeout for the kernel's reply.
    async fn query<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command,
    ) -> Result<T, Error> {
        let (reply, reply_rx) = oneshot::channel();
        self.send(Signal::Command(make(reply)))?;
        tokio::time::timeout(QUERY_TIMEOUT, reply_rx)
            .await
            .map_err(|_| Error::unavailable())?
            .map_err(|_| Error::unavailable())?
    }

    /// Whether the saved conversation or inbox carries this accepted input.
    /// Used to resolve a receipt left pending by a host crash.
    pub(crate) async fn has_request(
        &self,
        companion: String,
        request_id: String,
    ) -> Result<bool, Error> {
        self.query(|reply| Command::HasRequest {
            companion,
            request_id,
            reply,
        })
        .await
    }

    /// Asks the kernel to stop the companion's running turn. The stop is
    /// accepted here; its end arrives as a finished turn.
    pub(crate) fn stop(&self, companion: String, reason: Option<String>) -> Result<(), Error> {
        self.send(Signal::Command(Command::Stop { companion, reason }))
    }

    pub(crate) fn hold_recall(
        &self,
        companion: String,
        request_id: String,
        deadline: std::time::Instant,
    ) -> Result<(), Error> {
        self.send(Signal::Command(Command::HoldRecall {
            companion,
            request_id,
            deadline,
            reply: None,
        }))
    }

    /// Test/observation variant: resolves once the kernel has queued HoldOf
    /// for this input's model request.
    #[cfg(test)]
    pub(crate) fn hold_recall_with_ack(
        &self,
        companion: String,
        request_id: String,
        deadline: std::time::Instant,
    ) -> Result<oneshot::Receiver<()>, Error> {
        let (reply, ack) = oneshot::channel();
        self.send(Signal::Command(Command::HoldRecall {
            companion,
            request_id,
            deadline,
            reply: Some(reply),
        }))?;
        Ok(ack)
    }

    pub(crate) fn complete_recall(&self, request_id: String, prompt: String) -> Result<(), Error> {
        self.send(Signal::Command(Command::RecallCompleted {
            request_id,
            prompt,
        }))
    }

    pub(crate) fn cancel_recall(&self, request_id: String) -> Result<(), Error> {
        self.send(Signal::Command(Command::RecallCancelled { request_id }))
    }

    #[cfg(test)]
    fn signal_sender_for_test(&self) -> Sender<Signal> {
        self.signals.lock().expect("kernel signals").clone()
    }

    pub(crate) async fn history(
        &self,
        companion: String,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<w::History, Error> {
        self.query(|reply| Command::History {
            companion,
            cursor,
            limit,
            reply,
        })
        .await
    }

    pub(crate) async fn history_after(
        &self,
        companion: String,
        after: u64,
        limit: usize,
    ) -> Result<w::History, Error> {
        self.query(|reply| Command::HistoryAfter {
            companion,
            after,
            limit,
            reply,
        })
        .await
    }

    pub(crate) async fn message(&self, companion: String, id: String) -> Result<w::Message, Error> {
        self.query(|reply| Command::Message {
            companion,
            id,
            reply,
        })
        .await
    }

    /// Stops the turns, saves the session and returns once the app is gone.
    pub(crate) async fn shutdown(&self) -> Result<(), Error> {
        let thread = self.thread.lock().map_err(|_| Error::unavailable())?.take();
        let Some(thread) = thread else {
            return Ok(());
        };
        let (reply, reply_rx) = oneshot::channel();
        if self
            .send(Signal::Command(Command::Shutdown { reply }))
            .is_ok()
        {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, reply_rx).await;
        }
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        Ok(())
    }
}

impl Drop for Runtime {
    /// A dropped host must not leave a kernel thread drawing frames forever.
    /// The normal path is [`Runtime::shutdown`]; this covers a host that was
    /// dropped without stopping, and joins the thread with it.
    fn drop(&mut self) {
        if let Ok(signals) = self.signals.lock() {
            let (reply, _) = oneshot::channel();
            let _ = signals.send(Signal::Command(Command::Shutdown { reply }));
        }
        if let Ok(mut thread) = self.thread.lock()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::catalog::Catalog;
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_ecs::effects::Handler;
    use rig_ecs::prelude::{Tool, ToolContext, Update};
    use rig_ecs::tools::AppToolsExt;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn connection_of(model: MockCompletionModel) -> rig_ecs::model::Connection {
        let spec = Catalog::builtin()
            .resolve("deepseek/deepseek-flash")
            .expect("builtin catalog")
            .shared();
        let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(spec.reference(), model));
        rig_ecs::model::Connection {
            spec,
            handler: Handler(handler),
        }
    }

    fn connection(turns: Vec<Vec<MockStreamEvent>>) -> rig_ecs::model::Connection {
        connection_of(MockCompletionModel::from_stream_turns(turns))
    }

    fn start(directory: std::path::PathBuf) -> (Runtime, tokio_mpsc::UnboundedReceiver<Event>) {
        Runtime::start_with(directory, |_| {}).expect("kernel starts")
    }

    async fn next(received: &mut tokio_mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .expect("kernel event within 5s")
            .expect("kernel events open")
    }

    /// One turn: the scripted model asks for the harmless tool, then answers
    /// with its result in the second round. The conversation, tool result and
    /// final reply are in the session log, and a restarted kernel reads them.
    #[tokio::test]
    async fn a_turn_runs_the_tool_and_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("rig");
        let (runtime, mut received) = start(directory.clone());
        runtime
            .connect(
                "companion".into(),
                "日本語で簡潔に答える。".into(),
                connection(vec![
                    vec![
                        MockStreamEvent::tool_call("call-1", "current_time", json!({})),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                    vec![
                        MockStreamEvent::text("いまは午後です。"),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                ]),
                false,
            )
            .unwrap();
        runtime
            .deliver("companion".into(), "今何時？".into(), "request-1".into())
            .await
            .unwrap();

        let mut started = false;
        let finished = loop {
            match next(&mut received).await {
                Event::TurnStarted {
                    request_id,
                    messages,
                    ..
                } => {
                    assert!(!started);
                    started = true;
                    assert_eq!(request_id.as_deref(), Some("request-1"));
                    assert_eq!(
                        messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
                        ["今何時？"]
                    );
                }
                Event::TurnFinished {
                    state,
                    failure,
                    messages,
                    message_id,
                    ..
                } => break (state, failure, messages, message_id),
                Event::Delta { .. } => {}
                other => panic!("unexpected event: {other:?}"),
            }
        };
        let (state, failure, messages, message_id) = finished;
        assert_eq!(state, "answered");
        assert!(failure.is_none(), "{failure:?}");
        assert_eq!(
            messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["いまは午後です。"]
        );
        assert_eq!(message_id.as_deref(), Some(messages[0].id.as_str()));

        let history = runtime.history("companion".into(), None, 50).await.unwrap();
        assert_eq!(
            history
                .items
                .iter()
                .map(|m| (m.role.as_str(), m.text.as_str()))
                .collect::<Vec<_>>(),
            [("user", "今何時？"), ("assistant", "いまは午後です。")]
        );
        assert_eq!(history.items[0].request_id.as_deref(), Some("request-1"));
        runtime.shutdown().await.unwrap();

        let log = std::fs::read_to_string(directory.join("logs/companion.jsonl")).unwrap();
        assert!(log.contains("current_time"), "the tool call was not saved");
        assert!(log.contains("いまは午後です。"), "the reply was not saved");

        // A new kernel on the same directory restores the same conversation.
        let (restored, _received) = start(directory);
        let history = restored
            .history("companion".into(), None, 50)
            .await
            .unwrap();
        assert_eq!(
            history
                .items
                .iter()
                .map(|m| (m.role.as_str(), m.text.as_str()))
                .collect::<Vec<_>>(),
            [("user", "今何時？"), ("assistant", "いまは午後です。")]
        );
        restored.shutdown().await.unwrap();
    }

    #[derive(serde::Deserialize)]
    struct NoArgs {}

    /// A tool that tells the test it started and waits for the test to let
    /// it finish, so an input can arrive while the turn is running.
    struct Hold {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl Tool for Hold {
        const NAME: &'static str = "hold";
        type Args = NoArgs;
        type Output = String;
        type Error = std::convert::Infallible;

        fn description(&self) -> String {
            "Waits until released.".into()
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn call(&self, _: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
            let _ = self.entered.send(());
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = release.await;
            }
            Ok("released".into())
        }
    }

    fn user_texts(request: &rig_core::completion::CompletionRequest) -> Vec<String> {
        request
            .chat_history
            .iter()
            .filter_map(|message| message.rag_text())
            .collect()
    }

    #[tokio::test]
    async fn an_input_sent_during_a_turn_reaches_the_model_rather_than_being_lost() {
        let temp = tempfile::tempdir().unwrap();
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call("call-1", "hold", json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("一つ目を受け取りました。"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            // If the added input arrives after the second call was admitted,
            // it starts the next turn instead; either way it is answered.
            vec![
                MockStreamEvent::text("二つ目も受け取りました。"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let inspector = model.clone();
        let (runtime, mut received) = Runtime::start_with(temp.path().join("rig"), move |app| {
            app.add_tool(Hold {
                entered,
                release: std::sync::Mutex::new(Some(release_rx)),
            });
        })
        .unwrap();
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection_of(model),
                false,
            )
            .unwrap();
        runtime
            .deliver("companion".into(), "一つ目".into(), "request-1".into())
            .await
            .unwrap();
        loop {
            if let Event::TurnStarted { .. } = next(&mut received).await {
                break;
            }
        }
        entered_rx.recv().await.expect("the tool started");
        // The turn is busy in the tool call; this input must be kept rather
        // than dropped, and reach the model as the next request's input.
        runtime
            .deliver("companion".into(), "二つ目".into(), "request-2".into())
            .await
            .unwrap();
        assert!(
            runtime
                .has_request("companion".into(), "request-2".into())
                .await
                .unwrap(),
            "the accepted input is not tracked in the inbox"
        );
        assert!(
            !runtime
                .has_request("companion".into(), "never-sent".into())
                .await
                .unwrap()
        );
        release.send(()).unwrap();
        let mut finished = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), received.recv()).await {
                Ok(Some(Event::TurnFinished { state, failure, .. })) => {
                    finished += 1;
                    assert!(
                        state == "answered" || state == "stopped",
                        "unexpected turn end: {failure:?}"
                    );
                    if inspector.requests().iter().any(|request| {
                        user_texts(request)
                            .iter()
                            .any(|text| text.contains("二つ目"))
                    }) {
                        break;
                    }
                    assert!(finished < 3, "the added input never reached the model");
                }
                Ok(Some(Event::StorageFailed { reason })) | Ok(Some(Event::Fatal { reason })) => {
                    panic!("kernel failed: {reason}")
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        let requests = inspector.requests();
        assert!(
            requests.iter().any(|request| user_texts(request)
                .iter()
                .any(|text| text.contains("二つ目"))),
            "the added input was lost: {requests:?}"
        );
        let history = runtime.history("companion".into(), None, 50).await.unwrap();
        let users = history
            .items
            .iter()
            .filter(|message| message.role == "user")
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(users, ["一つ目", "二つ目"]);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stop_ends_a_turn_while_its_tool_is_still_running() {
        let temp = tempfile::tempdir().unwrap();
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (runtime, mut received) = Runtime::start_with(temp.path().join("rig"), move |app| {
            app.add_tool(Hold {
                entered,
                release: std::sync::Mutex::new(Some(release_rx)),
            });
        })
        .unwrap();
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection(vec![vec![
                    MockStreamEvent::tool_call("call-1", "hold", json!({})),
                    MockStreamEvent::final_response_with_default_usage(),
                ]]),
                false,
            )
            .unwrap();
        runtime
            .deliver("companion".into(), "止めて".into(), "request-1".into())
            .await
            .unwrap();
        loop {
            if let Event::TurnStarted { .. } = next(&mut received).await {
                break;
            }
        }
        entered_rx.recv().await.expect("the tool started");
        let started = Instant::now();
        runtime.stop("companion".into(), None).unwrap();
        loop {
            match next(&mut received).await {
                Event::TurnFinished { state, failure, .. } => {
                    assert_eq!(state, "stopped");
                    assert_eq!(
                        failure.as_ref().map(|failure| failure.code.as_str()),
                        Some("cancelled")
                    );
                    break;
                }
                Event::StorageFailed { reason } | Event::Fatal { reason } => {
                    panic!("kernel failed: {reason}")
                }
                _ => {}
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a stop waited for the running tool"
        );
        // The gate is never released: the stopped call must not keep the
        // kernel from stopping.
        tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
            .await
            .expect("the kernel stopped")
            .unwrap();
    }

    #[tokio::test]
    async fn mind_tool_calls_are_bound_to_their_own_companion() {
        let temp = tempfile::tempdir().unwrap();
        let (calls, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = std::sync::Arc::new(super::mind::MindBridge::new(calls));
        let (runtime, mut received) = Runtime::start_with(temp.path().join("rig"), {
            let bridge = bridge.clone();
            move |app| {
                app.insert_resource((*bridge).clone());
                app.add_tool(super::mind::MindUpdate(bridge.clone()));
                app.add_tool(super::mind::MindRead(bridge.clone()));
                app.add_tool(super::mind::MindRecall(bridge));
            }
        })
        .unwrap();
        let script = || {
            MockCompletionModel::from_stream_turns([
                vec![
                    MockStreamEvent::tool_call(
                        "call-mind",
                        "mind_update",
                        json!({"expected_revision": 1, "memories": []}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("了解しました。"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ])
        };
        runtime
            .connect(
                "companion-a".into(),
                "A".into(),
                connection_of(script()),
                true,
            )
            .unwrap();
        runtime
            .deliver("companion-a".into(), "Aの話".into(), "request-a".into())
            .await
            .unwrap();
        // Connecting a second parent must not replace the first one once its
        // conversation carries the accepted input.
        runtime
            .connect(
                "companion-b".into(),
                "B".into(),
                connection_of(script()),
                true,
            )
            .unwrap();
        runtime
            .deliver("companion-b".into(), "Bの話".into(), "request-b".into())
            .await
            .unwrap();
        let mut seen = std::collections::BTreeMap::new();
        for _ in 0..2 {
            let call = tokio::time::timeout(Duration::from_secs(5), calls_rx.recv())
                .await
                .expect("mind call")
                .expect("mind channel");
            assert!(!call.scope.sources.is_empty());
            assert!(
                call.scope
                    .turn_start
                    .starts_with(|c: char| c.is_ascii_digit())
                    || !call.scope.turn_start.is_empty()
            );
            seen.insert(
                call.scope.companion.clone(),
                call.scope.operation_id().to_owned(),
            );
            call.reply.send(Ok("ok".into())).unwrap();
        }
        assert!(seen.contains_key("companion-a"));
        assert!(seen.contains_key("companion-b"));
        assert_ne!(seen["companion-a"], seen["companion-b"]);
        for _ in 0..2 {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match next(&mut received).await {
                        Event::TurnFinished { .. } => break,
                        Event::StorageFailed { reason } | Event::Fatal { reason } => {
                            panic!("kernel failed: {reason}")
                        }
                        _ => {}
                    }
                }
            })
            .await;
        }
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_mind_prompt_change_reaches_the_next_model_call_without_a_reconnect() {
        use rig_core::completion::Message as RigMessage;
        let temp = tempfile::tempdir().unwrap();
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (runtime, mut received) = Runtime::start_with(temp.path().join("rig"), move |app| {
            app.add_tool(Hold {
                entered,
                release: std::sync::Mutex::new(Some(release_rx)),
            });
        })
        .unwrap();
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call("call-1", "hold", json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("新しい心で答えます。"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let captured = model.clone();
        runtime
            .connect(
                "companion".into(),
                "古い心".into(),
                connection_of(model),
                false,
            )
            .unwrap();
        runtime
            .deliver("companion".into(), "続けて".into(), "request-1".into())
            .await
            .unwrap();
        loop {
            if let Event::TurnStarted { .. } = next(&mut received).await {
                break;
            }
        }
        entered_rx.recv().await.expect("the tool started");
        runtime
            .set_mind_prompt("companion".into(), "新しい心".into())
            .unwrap();
        // Let the command reach the kernel's next frame while the call waits.
        tokio::time::sleep(Duration::from_millis(100)).await;
        release.send(()).unwrap();
        loop {
            match next(&mut received).await {
                Event::TurnFinished { state, .. } => {
                    assert_eq!(state, "answered");
                    break;
                }
                Event::StorageFailed { reason } | Event::Fatal { reason } => {
                    panic!("kernel failed: {reason}")
                }
                _ => {}
            }
        }
        let requests = captured.requests();
        assert_eq!(
            requests.len(),
            2,
            "the prompt change reconnected or retried"
        );
        let system = |request: &rig_core::completion::CompletionRequest| {
            request
                .chat_history
                .iter()
                .find_map(|message| match &**message {
                    RigMessage::System { content } => Some(content.clone()),
                    _ => None,
                })
        };
        assert_eq!(system(&requests[0]).as_deref(), Some("古い心"));
        assert_eq!(
            system(&requests[1]).as_deref(),
            Some("新しい心"),
            "the next model call did not see the committed mind"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_idle_kernel_does_not_draw_frames_continuously() {
        let temp = tempfile::tempdir().unwrap();
        let frames = Arc::new(AtomicUsize::new(0));
        let counter = frames.clone();
        let (runtime, _received) = Runtime::start_with(temp.path().join("rig"), move |app| {
            app.add_systems(Update, move || {
                counter.fetch_add(1, Ordering::Relaxed);
            });
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let drawn = frames.load(Ordering::Relaxed);
        assert!(drawn < 10, "an idle kernel drew {drawn} frames in 400ms");
        runtime.shutdown().await.unwrap();
    }

    /// The production model path: the built-in OpenAI Responses wire through
    /// the host's authorized transport. The server is a local fixture and the
    /// credential is a test value; no model provider is contacted.
    #[tokio::test]
    async fn the_openai_responses_wire_sends_with_the_host_credential() {
        use crate::domain::OperationKind;
        use crate::llm::{LlmProvider, ProviderBinding};
        use crate::ports::Secret;
        use std::sync::{Arc, Mutex};

        struct PolicyOnly(crate::llm::Policy);
        impl LlmProvider for PolicyOnly {
            fn policy(
                &self,
                _: &crate::domain::Connection,
            ) -> crate::ports::Result<crate::llm::Policy> {
                Ok(self.0.clone())
            }
            fn generate(
                &self,
                _: crate::llm::Invocation,
            ) -> futures_util::future::BoxFuture<
                '_,
                crate::ports::Result<
                    futures_util::stream::BoxStream<
                        'static,
                        crate::ports::Result<crate::llm::Event>,
                    >,
                >,
            > {
                Box::pin(async { Err(crate::domain::Error::unavailable()) })
            }
            fn models(
                &self,
                _: crate::llm::Invocation,
            ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<Vec<crate::llm::Model>>>
            {
                Box::pin(async { Ok(Vec::new()) })
            }
        }
        struct Allow;
        impl crate::llm::Authorization for Allow {
            fn check(&self) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{origin}/v1");
        let terminal = json!({
            "id": "resp_1", "object": "response", "model": "gpt-6-luna",
            "status": "completed",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "pong", "annotations": [], "logprobs": []}]}],
            "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6},
        });
        let events = [
            json!({"type": "response.created", "sequence_number": 0, "response": {"id":"resp_1","object":"response","model":"gpt-6-luna","status":"in_progress","output":[],"usage":null}}),
            json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": {"type":"message","id":"msg_1","content":[]}}),
            json!({"type": "response.output_text.delta", "sequence_number": 2, "output_index": 0, "content_index": 0, "delta": "pong"}),
            json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0, "item": {"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"pong","annotations":[],"logprobs":[]}]}}),
            json!({"type": "response.completed", "sequence_number": 4, "response": terminal}),
        ];
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        let service = axum::Router::new().fallback(axum::routing::post(
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let recorded = recorded.clone();
                let body = body.clone();
                async move {
                    recorded.lock().unwrap().push((
                        uri.path().to_owned(),
                        headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned(),
                    ));
                    axum::response::Response::builder()
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            },
        ));
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });

        let connection = crate::domain::Connection {
            id: "local".into(),
            kind: "llm".into(),
            adapter: "openai".into(),
            endpoint: endpoint.clone(),
            ..Default::default()
        };
        let policy = crate::llm::Policy {
            revision: "test".into(),
            fingerprint: json!({"local": true}),
            destinations: vec![origin],
            requires_credentials: true,
            environment_variable: None,
        };
        let binding =
            ProviderBinding::resolve(Arc::new(PolicyOnly(policy.clone())), &connection).unwrap();
        let outbound = Arc::new(crate::llm::Outbound::new(
            "test".into(),
            connection,
            Some(Secret {
                value: "test-key".into(),
                nonce: "n".into(),
            }),
            Arc::new(Allow),
            binding,
            OperationKind::Conversation,
        ));
        let model = crate::infrastructure::llm::agent_connection(outbound, "gpt-6-luna").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (runtime, mut received) = start(temp.path().join("rig"));
        runtime
            .connect("companion".into(), "簡潔に答える。".into(), model, false)
            .unwrap();
        runtime
            .deliver("companion".into(), "ping".into(), "request-1".into())
            .await
            .unwrap();
        let answer = loop {
            match next(&mut received).await {
                Event::TurnFinished {
                    state,
                    messages,
                    failure,
                    ..
                } => {
                    assert_eq!(state, "answered", "{failure:?}");
                    break messages
                        .iter()
                        .map(|message| message.text.clone())
                        .collect::<Vec<_>>();
                }
                Event::StorageFailed { reason } | Event::Fatal { reason } => {
                    panic!("kernel failed: {reason}")
                }
                _ => {}
            }
        };
        assert_eq!(answer, ["pong"]);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "/v1/responses");
        assert_eq!(seen[0].1, "Bearer test-key");
        runtime.shutdown().await.unwrap();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_log_write_failure_is_reported_as_unhealthy() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("rig");
        let (runtime, mut received) = start(directory.clone());
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection(vec![vec![
                    MockStreamEvent::text("一度目"),
                    MockStreamEvent::final_response_with_default_usage(),
                ]]),
                false,
            )
            .unwrap();
        runtime
            .deliver("companion".into(), "一度目".into(), "request-1".into())
            .await
            .unwrap();
        loop {
            if let Event::TurnFinished { .. } = next(&mut received).await {
                break;
            }
        }
        runtime.shutdown().await.unwrap();

        let log = directory.join("logs/companion.jsonl");
        let mut permissions = std::fs::metadata(&log).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&log, permissions).unwrap();
        let (runtime, mut received) = start(directory);
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection(vec![vec![
                    MockStreamEvent::text("二度目"),
                    MockStreamEvent::final_response_with_default_usage(),
                ]]),
                false,
            )
            .unwrap();
        // Acceptance waits for the log write: an input whose save failed is
        // refused, not accepted and then lost.
        let refused = runtime
            .deliver("companion".into(), "二度目".into(), "request-2".into())
            .await;
        assert_eq!(
            refused.unwrap_err().kind,
            crate::domain::ErrorKind::StorageFailure
        );
        let failure = loop {
            if let Event::StorageFailed { reason } = next(&mut received).await {
                break reason;
            }
        };
        assert!(!failure.is_empty());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_oversized_single_reply_is_refused_and_the_input_is_returned() {
        let limit = crate::domain::OperationKind::Conversation
            .spec()
            .output_bytes;
        let temp = tempfile::tempdir().unwrap();
        let (runtime, mut received) = start(temp.path().join("rig"));
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection(vec![vec![
                    MockStreamEvent::text("x".repeat(limit + 1)),
                    MockStreamEvent::final_response_with_default_usage(),
                ]]),
                false,
            )
            .unwrap();
        runtime
            .deliver(
                "companion".into(),
                "長すぎる返答をください".into(),
                "request-1".into(),
            )
            .await
            .unwrap();
        let mut recalled = None;
        let finished = loop {
            match next(&mut received).await {
                Event::Recalled { text, .. } => recalled = Some(text),
                Event::TurnFinished {
                    state,
                    failure,
                    messages,
                    ..
                } => break (state, failure, messages),
                Event::StorageFailed { reason } | Event::Fatal { reason } => {
                    panic!("kernel failed: {reason}")
                }
                _ => {}
            }
        };
        let (state, failure, messages) = finished;
        assert_eq!(state, "failed");
        assert_eq!(
            failure.as_ref().map(|failure| failure.code.as_str()),
            Some("output_limit")
        );
        assert!(
            messages.iter().all(|message| message.role != "assistant"),
            "the oversized reply was announced as a message: {messages:?}"
        );
        // The input is handed back after the failed turn settles.
        if recalled.is_none() {
            recalled = loop {
                match tokio::time::timeout(Duration::from_secs(2), received.recv()).await {
                    Ok(Some(Event::Recalled { text, .. })) => break Some(text),
                    Ok(Some(Event::StorageFailed { reason } | Event::Fatal { reason })) => {
                        panic!("kernel failed: {reason}")
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break None,
                }
            };
        }
        assert_eq!(recalled.as_deref(), Some("長すぎる返答をください"));
        let history = runtime.history("companion".into(), None, 50).await.unwrap();
        assert!(
            history.items.is_empty(),
            "a refused reply or its withdrawn input was saved: {:?}",
            history.items
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_result_arriving_before_the_model_request_is_not_lost() {
        let temp = tempfile::tempdir().unwrap();
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("早い結果で答えました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let requests = model.clone();
        let (runtime, mut received) = start(temp.path().join("rig"));
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection_of(model),
                false,
            )
            .unwrap();
        runtime
            .hold_recall(
                "companion".into(),
                "early-request".into(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        let sender = runtime.signal_sender_for_test();
        sender
            .send(Signal::Command(Command::RecallCompleted {
                request_id: "early-request".into(),
                prompt: "EARLY-RECALL".into(),
            }))
            .unwrap();
        runtime
            .deliver(
                "companion".into(),
                "こんにちは".into(),
                "early-request".into(),
            )
            .await
            .unwrap();
        let finished = loop {
            match next(&mut received).await {
                Event::TurnFinished { state, failure, .. } => break (state, failure),
                Event::TurnStarted { .. } => {}
                _ => {}
            }
        };
        assert_eq!(finished.0, "answered");
        assert!(finished.1.is_none(), "{:?}", finished.1);
        assert_eq!(requests.requests().len(), 1);
        let prompt = requests.requests()[0]
            .chat_history
            .iter()
            .find_map(|message| match &**message {
                rig_core::completion::Message::System { content } => Some(content.clone()),
                _ => None,
            })
            .expect("system prompt");
        assert!(
            prompt.contains("EARLY-RECALL"),
            "an early result was lost: {prompt}"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_held_model_call_is_released_by_async_completion() {
        let temp = tempfile::tempdir().unwrap();
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("非同期結果を使って答えました。"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let requests = model.clone();
        let (runtime, mut received) = start(temp.path().join("rig"));
        runtime
            .connect(
                "companion".into(),
                "簡潔に答える。".into(),
                connection_of(model),
                false,
            )
            .unwrap();
        let held = runtime
            .hold_recall_with_ack(
                "companion".into(),
                "hold-request".into(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        runtime
            .deliver(
                "companion".into(),
                "こんにちは".into(),
                "hold-request".into(),
            )
            .await
            .unwrap();
        loop {
            match next(&mut received).await {
                Event::TurnStarted { .. } => break,
                Event::TurnFinished { .. } => panic!("turn ended before the hold was created"),
                _ => {}
            }
        }
        tokio::time::timeout(Duration::from_secs(2), held)
            .await
            .expect("the kernel observed the hold within 2s")
            .expect("the hold ack");
        assert_eq!(
            requests.requests().len(),
            0,
            "the model was called while the hold was active"
        );

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<String>();
        let sender = runtime.signal_sender_for_test();
        tokio::spawn(async move {
            let Ok(prompt) = result_rx.await else {
                return;
            };
            let _ = sender.send(Signal::Command(Command::RecallCompleted {
                request_id: "hold-request".into(),
                prompt,
            }));
        });
        result_tx.send("SYSTEM\nRECALL-FIXED-42".into()).unwrap();
        let finished = loop {
            match next(&mut received).await {
                Event::TurnFinished {
                    state,
                    failure,
                    messages,
                    ..
                } => break (state, failure, messages),
                Event::TurnStarted { .. } => {}
                _ => {}
            }
        };
        assert_eq!(finished.0, "answered");
        assert!(finished.1.is_none(), "{:?}", finished.1);
        assert_eq!(requests.requests().len(), 1);
        let request = &requests.requests()[0];
        let prompt = request
            .chat_history
            .iter()
            .find_map(|message| match &**message {
                rig_core::completion::Message::System { content } => Some(content.clone()),
                _ => None,
            })
            .expect("system prompt");
        assert!(
            prompt.contains("RECALL-FIXED-42"),
            "the async result was not in the model request: {prompt}"
        );
        // A second, late completion after the turn settled must not send a
        // second model call or start another turn.
        let late = runtime.signal_sender_for_test();
        late.send(Signal::Command(Command::RecallCompleted {
            request_id: "hold-request".into(),
            prompt: "LATE-RESULT".into(),
        }))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            requests.requests().len(),
            1,
            "a late result sent another model call"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), received.recv())
                .await
                .is_err(),
            "a late result started another turn"
        );

        // A second input held while the search is pending can still be
        // stopped; a completion arriving after the stop must not send.
        let stop_ack = runtime
            .hold_recall_with_ack(
                "companion".into(),
                "stop-request".into(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        runtime
            .deliver(
                "companion".into(),
                "止まる入力".into(),
                "stop-request".into(),
            )
            .await
            .unwrap();
        loop {
            match next(&mut received).await {
                Event::TurnStarted { request_id, .. }
                    if request_id.as_deref() == Some("stop-request") =>
                {
                    break;
                }
                Event::TurnStarted { .. } => {}
                _ => {}
            }
        }
        tokio::time::timeout(Duration::from_secs(2), stop_ack)
            .await
            .expect("the stop input was held")
            .expect("the stop hold ack");
        runtime
            .stop("companion".into(), Some("test stop".into()))
            .unwrap();
        let stopped = loop {
            if let Event::TurnFinished { state, .. } = next(&mut received).await {
                break state;
            }
        };
        assert_eq!(stopped, "stopped");
        let late = runtime.signal_sender_for_test();
        late.send(Signal::Command(Command::RecallCompleted {
            request_id: "stop-request".into(),
            prompt: "STALE-RESULT".into(),
        }))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            requests.requests().len(),
            1,
            "a stopped request was sent after a late completion"
        );
        runtime.shutdown().await.unwrap();
    }
}
