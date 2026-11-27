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

#[test]
fn every_cut_budget_keeps_a_complete_newest_request() {
    let history = conversation(4);
    let latest = history
        .entries()
        .rev()
        .find(rig_core::conversation::EntryRef::is_request)
        .unwrap()
        .id;
    let limit = history.start_of(latest);
    for budget in 0..=message_tokens(history.messages().iter().map(Arc::as_ref)) {
        if let Some(id) = cut(&history, budget, 0, limit) {
            let at = history.index_of(id).unwrap();
            assert!(history.entries().nth(at).unwrap().is_request());
            assert!(at <= history.index_of(latest).unwrap());
            History::from_messages(history.messages()[at..].iter().cloned())
                .validate()
                .expect("every kept result still has its call");
        }
    }
    assert_eq!(cut(&history, 0, 0, limit), Some(latest));
}

#[test]
fn fitting_cannot_split_the_latest_turn_between_its_inputs() {
    let mut history = History::new();
    let first = history.push(Message::user("first input ".repeat(100)));
    let second = history.push(Message::user("second input"));
    history.open_turn();
    assert_eq!(history.turn_start(), first);
    let mut view = history.request_view();
    let request = FitRequest {
        model: "",
        prefix: &empty_request(),
        anchor: None,
        sent: None,
        budget: 95,
        turn_start: Some(first),
    };
    assert!(!fit(&mut view, &request));
    assert_eq!(view.id(0), Some(first));
    assert_eq!(view.id(1), Some(second));
}

#[test]
fn a_large_old_summary_cannot_crowd_out_a_fitting_current_turn() {
    let mut saved = History::new();
    let earlier = saved.push(Message::user("earlier question"));
    let through = saved.push(Message::assistant("earlier answer"));
    let current = saved.push(Message::user("latest question"));
    let mut view = saved.request_view();
    inject_summary(
        &mut view,
        &Summary {
            text: "older material ".repeat(800),
            through_message_id: through.get(),
        },
    );
    assert!(is_summary_entry(view.entries().next()));
    let request = FitRequest {
        model: "",
        prefix: &empty_request(),
        anchor: None,
        sent: None,
        budget: 260,
        turn_start: Some(current),
    };
    assert!(fit(&mut view, &request));
    assert!(noticed(&view));
    assert_eq!(view.id(1), Some(current));
    assert_eq!(saved.id(0), Some(earlier));
    assert_eq!(saved.messages().len(), 3);
}

#[test]
fn a_model_input_cap_and_total_window_are_separate_constraints() {
    let mut spec = Catalog::builtin()
        .resolve("deepseek/deepseek-flash")
        .unwrap()
        .spec
        .clone();
    spec.context_window = Some(120_000);
    spec.max_output_tokens = Some(32_000);
    spec.max_input_tokens = Some(16_000);
    assert_eq!(input_budget(&spec, None), 14_400);
    spec.max_input_tokens = Some(90_000);
    assert_eq!(input_budget(&spec, None), 79_000);
}

#[test]
fn the_summary_cursor_covers_only_the_complete_turns_in_its_actual_request() {
    let mut history = History::new();
    history.push(Message::user("the first essential fact"));
    history.open_turn();
    let first_end = history.push(Message::assistant("first answer"));
    history.end_turn(TurnEnd::Answered { answer: None });
    history.push(Message::user("oversized second fact ".repeat(200)));
    history.open_turn();
    history.push(Message::assistant("second answer"));
    history.end_turn(TurnEnd::Answered { answer: None });
    history.push(Message::user("newest question"));
    history.open_turn();
    history.push(Message::assistant("newest answer"));
    history.end_turn(TurnEnd::Answered { answer: None });

    let spec = ModelSpec::new(ProviderId::catalog("openai").unwrap(), "summary-test")
        .with_context_window(1_200);
    let (through, job::SummaryInput::Single(request)) =
        job::plan_summary(&history, None, &spec).unwrap()
    else {
        panic!("the first complete turn fits in one request")
    };
    let serialized = serde_json::to_string(&request).unwrap();
    assert_eq!(through, first_end.get());
    assert!(serialized.contains("the first essential fact"));
    assert!(!serialized.contains("oversized second fact"));
    assert!(!serialized.contains("newest question"));

    let existing = Summary {
        text: "first summary".into(),
        through_message_id: through,
    };
    assert!(matches!(
        job::plan_summary(&history, Some(&existing), &spec),
        Ok((_, job::SummaryInput::Segments { .. }))
    ));
}

#[tokio::test]
async fn a_turn_larger_than_one_summary_request_is_covered_without_a_gap() {
    use rig_core::message::UserContent;

    let mut history = History::new();
    let input = format!(
        "start-of-old-turn {} end-of-old-turn",
        "長い日本語の発話 ".repeat(500)
    );
    history.push(Message::user(&input));
    history.open_turn();
    let through = history.push(Message::assistant("old reply"));
    history.end_turn(TurnEnd::Answered { answer: None });
    history.push(Message::user("latest question"));
    history.open_turn();
    history.push(Message::assistant("latest answer"));
    history.end_turn(TurnEnd::Answered { answer: None });
    let spec = ModelSpec::new(ProviderId::catalog("openai").unwrap(), "summary-test")
        .with_context_window(1_200);
    let model = MockCompletionModel::from_turns((0..32).map(|_| MockTurn::text("rolling summary")));
    let observed = model.clone();
    let result = job::summarize(&history, None, &spec, &model.into())
        .await
        .unwrap();
    assert_eq!(result.through_message_id, through.get());
    let requests = observed.requests();
    assert!(requests.len() > 1);
    let mut combined = String::new();
    for request in requests {
        let Message::User { content } = request.chat_history.last().unwrap().as_ref() else {
            panic!("the last message contains a transcript segment")
        };
        let UserContent::Text(prompt) = &content[0] else {
            panic!("the summary prompt is text")
        };
        let segment = prompt
            .text
            .split_once("<conversation>\n")
            .unwrap()
            .1
            .split_once("\n</conversation>")
            .unwrap()
            .0;
        combined.push_str(segment);
    }
    assert_eq!(
        combined,
        rig_memory::serialize_conversation(&history.messages()[..2])
    );

    let failing =
        MockCompletionModel::from_turns([MockTurn::text("first fragment"), MockTurn::text("")]);
    assert!(matches!(
        job::summarize(&history, None, &spec, &failing.into()).await,
        Err(job::CompactionError::Model(_))
    ));
}

#[test]
fn a_fit_keeps_tool_pairs_and_drops_whole_turns() {
    let mut history = conversation(8);
    history.validate().expect("a canonical fixture");
    assert!(message_tokens(history.messages().iter().map(Arc::as_ref)) > 550);
    assert!(fit(
        &mut history,
        &FitRequest {
            model: "",
            prefix: &empty_request(),
            anchor: None,
            sent: None,
            budget: 550,
            turn_start: None,
        },
    ));
    assert!(message_tokens(history.messages().iter().map(Arc::as_ref)) <= 550);
    history.validate().expect("tool calls keep their results");
    assert!(noticed(&history));
    let mut entries = history.entries();
    entries.next().expect("the notice");
    assert!(entries.next().expect("a kept entry").is_request());
}

#[test]
fn fitting_clears_large_tool_outputs_only_in_the_request_view() {
    let mut saved = History::new();
    saved.push(Message::user("read the file"));
    tool_turn(&mut saved, "read-1", "x".repeat(20_000));
    saved.push(Message::assistant("done"));
    let original = saved.messages().to_vec();
    let mut request = saved.view();

    assert!(fit(
        &mut request,
        &FitRequest {
            model: "",
            prefix: &empty_request(),
            anchor: None,
            sent: None,
            budget: 500,
            turn_start: None,
        },
    ));
    assert!(message_tokens(request.messages().iter().map(Arc::as_ref)) <= 500);
    assert_eq!(request.messages().len(), saved.messages().len());
    assert_ne!(request.messages(), saved.messages());
    assert_eq!(saved.messages(), original);
    request
        .validate()
        .expect("clearing keeps tool calls paired");
}

#[test]
fn a_fit_refuses_a_single_oversized_message() {
    let mut history = History::new();
    history.extend([
        Message::user("q".repeat(20_000)),
        Message::assistant("a short answer"),
    ]);
    assert!(!fit(
        &mut history,
        &FitRequest {
            model: "",
            prefix: &empty_request(),
            anchor: None,
            sent: None,
            budget: 900,
            turn_start: None,
        },
    ));
    assert!(message_tokens(history.messages().iter().map(Arc::as_ref)) > 900);
    assert_eq!(history.messages().len(), 2);
}

#[test]
fn measured_usage_applies_only_to_the_same_request_prefix() {
    let mut history = History::from_messages([Arc::new(Message::user("hello"))]);
    let anchor = rig_memory::Anchor {
        through: history.id(0).unwrap(),
        usage: rig_core::completion::Usage::default().total_tokens(501),
    };
    let prefix = empty_request();
    let mut request = FitRequest {
        model: "test/model",
        prefix: &prefix,
        anchor: Some(&anchor),
        sent: None,
        budget: 500,
        turn_start: None,
    };
    request.sent = Some(request.start(&history));
    assert!(!fit(&mut history, &request));

    let changed = CompletionRequest::new(Message::system("A different system prompt"));
    request.prefix = &changed;
    assert!(fit(&mut history, &request));
}

#[test]
fn the_fixed_request_weight_counts_against_the_window() {
    let prompt = "あなたはデスクトップ常駐AIコンパニオンです。".repeat(60);
    let mut prefix = CompletionRequest::new(Message::system(&prompt));
    prefix.tools.push(tool(&"t".repeat(2_000)));
    let mut history = conversation(6);
    assert!(message_tokens(history.messages().iter().map(Arc::as_ref)) <= 1_100);
    assert!(!fit(
        &mut history,
        &FitRequest {
            model: "test/model",
            prefix: &prefix,
            anchor: None,
            sent: None,
            budget: 1_100,
            turn_start: None,
        },
    ));
    assert!(noticed(&history), "the fixed parts can still refuse");
    let mut history = conversation(6);
    assert!(fit(
        &mut history,
        &FitRequest {
            model: "test/model",
            prefix: &prefix,
            anchor: None,
            sent: None,
            budget: 1_900,
            turn_start: None,
        },
    ));
    assert!(
        rig_memory::request_estimate(&prefix)
            + message_tokens(history.messages().iter().map(Arc::as_ref))
            <= 1_900
    );
    assert!(noticed(&history));
}

#[test]
fn persisted_summaries_resume_without_rewriting_source_messages() {
    let temp = tempfile::tempdir().unwrap();
    let store = rig_memory::DirStore::open(temp.path()).unwrap();
    let mut source = conversation(3);
    let original_messages = source.messages().to_vec();
    let original_ids: Vec<_> = source.entries().map(|entry| entry.id).collect();
    let older = Summary {
        text: "one completed turn".into(),
        through_message_id: original_ids[3].get(),
    };
    source.add_aside(Aside::new(
        SUMMARY,
        Author::plugin("ene-host"),
        serde_json::to_value(&older).unwrap(),
    ));
    rig_memory::save(&store, "main", &mut source).unwrap();
    let older_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
    let mut source = rig_memory::load(&store, "main").unwrap();
    assert_eq!(summary_of(&source), Some(older));

    let latest = Summary {
        text: "two completed turns".into(),
        through_message_id: original_ids[7].get(),
    };
    source.add_aside(Aside::new(
        SUMMARY,
        Author::plugin("ene-host"),
        serde_json::to_value(&latest).unwrap(),
    ));
    rig_memory::save(&store, "main", &mut source).unwrap();
    let latest_bytes = rig_memory::Store::read_log(&store, "main").unwrap();
    assert!(latest_bytes.len() > older_bytes.len());
    assert!(latest_bytes.starts_with(&older_bytes));
    drop(store);

    let store = rig_memory::DirStore::open(temp.path()).unwrap();
    let source = rig_memory::load(&store, "main").unwrap();
    let resumed = summary_of(&source).expect("the newest saved summary");
    assert_eq!(resumed, latest);
    assert_eq!(source.asides().count(), 2);
    assert_eq!(source.messages(), original_messages);
    assert_eq!(
        source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
        original_ids
    );

    let mut request = source.view();
    inject_summary(&mut request, &resumed);
    assert!(matches!(
        request.entries().next().unwrap().message,
        Message::System { content } if content.contains("two completed turns")
            && !content.contains("one completed turn")
    ));
    assert_eq!(&request.messages()[1..], &original_messages[8..]);
    assert_eq!(
        request
            .entries()
            .skip(1)
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        original_ids[8..]
    );
    request
        .validate()
        .expect("the kept tool call has its result");
    assert_eq!(source.messages(), original_messages);
    assert_eq!(
        source.entries().map(|entry| entry.id).collect::<Vec<_>>(),
        original_ids
    );

    let persisted = rig_memory::load(&store, "main").unwrap();
    assert_eq!(persisted, source);
    assert_eq!(persisted.messages(), original_messages);
    assert_eq!(
        persisted
            .entries()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(summary_of(&persisted), Some(latest));
    assert_eq!(
        rig_memory::Store::read_log(&store, "main").unwrap(),
        latest_bytes
    );
}

#[test]
fn injected_blocks_are_system_and_never_two_user_messages() {
    let mut history = conversation(6);
    let through = history.entries().nth(3).unwrap().id.get();
    let summary = Summary {
        text: "これまでの流れ: テスト".to_owned(),
        through_message_id: through,
    };
    inject_summary(&mut history, &summary);
    history.validate().expect("the summary keeps tool pairs");
    assert!(head_text(&history).starts_with(SUMMARY_HEAD));
    assert!(fit(
        &mut history,
        &FitRequest {
            model: "",
            prefix: &empty_request(),
            anchor: None,
            sent: None,
            budget: 650,
            turn_start: None,
        },
    ));
    history.validate().expect("the notice keeps tool pairs");
    assert!(
        head_text(&history).starts_with(SUMMARY_HEAD),
        "the summary head is kept"
    );
    let messages: Vec<&Message> = history.messages().iter().map(Arc::as_ref).collect();
    for pair in messages.windows(2) {
        assert!(
            !(matches!(pair[0], Message::User { .. }) && matches!(pair[1], Message::User { .. })),
            "injection never puts two user messages in a row"
        );
    }
    let notices = history
        .entries()
        .filter(|entry| matches!(entry.message, Message::System { content } if content == NOTICE))
        .count();
    assert_eq!(notices, 1, "the notice is a system message");
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

/// One companion holding `conversation`, and the resources a job reads.
struct SummaryWorld {
    app: App,
    agent: Entity,
    companion: String,
    /// Kept alive: the state directory and the runtime the summary runs on.
    _root: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
}

impl SummaryWorld {
    /// A world whose model endpoint is closed, so a summary that did run
    /// never reaches the network. Only the start runs: the threshold tests
    /// read the slot it fills.
    fn new(conversation: History, context_window: u32) -> Self {
        Self::at(
            conversation,
            context_window,
            context_window,
            "http://127.0.0.1:9",
        )
    }

    /// `threshold_window` is the room the agent's own model reports, which
    /// is what the trigger measures the conversation against;
    /// `summary_window` is the room the summarizer's keep window is
    /// measured in, so a test can cut the conversation where it wants.
    fn at(conversation: History, threshold_window: u32, summary_window: u32, base: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        store
            .settings_repo()
            .update_settings(
                store.settings_repo().settings_revision().unwrap(),
                &[(connection_row(base), 0)],
            )
            .unwrap();
        let companion = companion();
        store.companion_repo().insert_companion(&companion).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let credentials = Credentials::new(true);
        credentials.remember("c1", "test-key".into());
        let mut app = App::new();
        app.insert_resource(store)
            .insert_resource(credentials)
            .insert_resource(models(summary_window))
            .insert_resource(Embeddings::default())
            .insert_resource(HostRuntime(runtime.handle().clone()))
            .init_resource::<CompactionState>()
            .add_systems(Update, summarize_at_turn_boundary);
        let agent = app
            .world_mut()
            .spawn((
                CompanionAgent {
                    companion_id: companion.id.clone(),
                },
                Conversation(conversation),
                tiny_model(threshold_window),
            ))
            .id();
        Self {
            app,
            agent,
            companion: companion.id,
            _root: root,
            _runtime: runtime,
        }
    }

    /// Runs the whole job, both halves, until its slot is free again: the
    /// memories are committed and the summary is either saved or left out.
    fn run(&mut self) {
        self.app
            .add_systems(Update, collect_summaries.after(summarize_at_turn_boundary));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut free = 0;
        while std::time::Instant::now() < deadline {
            self.app.update();
            if self.app.world().resource::<CompactionState>().is_running() {
                free = 0;
            } else {
                free += 1;
                if free >= 2 {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the job did not settle");
    }

    /// The share of the companion's input limit its conversation holds.
    fn ratio(&self) -> f64 {
        let world = self.app.world();
        let conversation = world.get::<Conversation>(self.agent).unwrap();
        let limit = world
            .get::<Connection>(self.agent)
            .unwrap()
            .spec
            .input_limit()
            .unwrap();
        message_tokens(conversation.0.messages().iter().map(Arc::as_ref)) as f64 / f64::from(limit)
    }

    /// Whether a summary is in this companion's slot.
    fn summarizing(&self) -> bool {
        self.app
            .world()
            .resource::<CompactionState>()
            .companions
            .get(&self.companion)
            .is_some_and(|slot| slot.running.is_some())
    }
}

/// The catalog the summary and the pass resolve through: the connection's
/// model with `context_window` tokens of room, so the summarizer's keep
/// window is measured in the fixture's own tokens.
fn models(context_window: u32) -> Models {
    let provider = ProviderId::catalog("openrouter").unwrap();
    let mut catalog = Catalog::builtin().clone();
    catalog.insert(ModelSpec::new(provider, "gpt-4o-mini").with_context_window(context_window));
    Models(Connector::new(catalog))
}

/// The agent's own model: a mocked handler with `context_window` tokens of
/// room, so the threshold needs no conversation of that size.
fn tiny_model(context_window: u32) -> Connection {
    let provider = ProviderId::catalog("openai").unwrap();
    let spec = Arc::new(ModelSpec::new(provider, "test/tiny").with_context_window(context_window));
    let model: DynModel<Completion> =
        MockCompletionModel::from_turns([MockTurn::text("unused")]).into();
    let handler = rig_ecs::effects::Handler(ErasedHandler::new(ModelAdapter::<Completion>::new(
        spec.reference(),
        model,
    )));
    Connection { spec, handler }
}

/// The settings row the job connects through: an OpenAI adapter aimed at
/// `endpoint`.
fn connection_row(endpoint: &str) -> crate::host::settings::Connection {
    use crate::host::settings::{
        ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
    };
    crate::host::settings::Connection {
        id: ene_domain::ConnectionId::parse("c1").unwrap(),
        name: "main".into(),
        enabled: true,
        kind: ConnectionKind::Llm,
        config: ConnectionConfig::Llm(LlmConnection {
            endpoint: Endpoint {
                adapter: "openrouter".into(),
                endpoint: endpoint.into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            model_ids: vec!["gpt-4o-mini".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    }
}

fn companion() -> Companion {
    let now = ene_store::util::now();
    Companion {
        id: "ene".into(),
        name: "ene".into(),
        mind: "kind".into(),
        profile: Profile::default(),
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::empty(),
        voice: JsonDoc::empty(),
        assignments: ModelAssignments {
            compaction: Some(ModelAssignment {
                connection_id: "c1".into(),
                model_id: "gpt-4o-mini".into(),
            }),
            consolidation: Some(ModelAssignment {
                connection_id: "c1".into(),
                model_id: "gpt-4o-mini".into(),
            }),
            ..Default::default()
        },
        persona: None,
        voice_identifier: None,
        revision: 1,
        created_at: now.clone(),
        updated_at: now,
    }
}

#[test]
fn a_companion_over_the_threshold_at_a_turn_boundary_starts_a_summary() {
    let mut world = SummaryWorld::new(conversation(4), 400);
    assert!(
        world.ratio() >= THRESHOLD_RATIO,
        "the fixture is over the threshold: {}",
        world.ratio()
    );
    world.app.update();
    assert!(
        world.summarizing(),
        "the threshold with no turn in flight starts a summary"
    );
}

#[test]
fn a_companion_under_the_threshold_at_a_turn_boundary_does_not_start_a_summary() {
    let mut world = SummaryWorld::new(conversation(4), 4_000);
    assert!(
        world.ratio() < THRESHOLD_RATIO,
        "the fixture is under the threshold: {}",
        world.ratio()
    );
    world.app.update();
    assert!(!world.summarizing(), "a turn boundary alone starts nothing");
}

#[test]
fn a_companion_over_the_threshold_mid_turn_does_not_start_a_summary() {
    let mut world = SummaryWorld::new(conversation(4), 400);
    assert!(
        world.ratio() >= THRESHOLD_RATIO,
        "the fixture is over the threshold: {}",
        world.ratio()
    );
    let agent = world.agent;
    world.app.world_mut().spawn(TurnOf(agent));
    assert!(
        world.app.world().get::<ActiveTurn>(agent).is_some(),
        "the turn in flight is the signal the summary waits for"
    );
    world.app.update();
    assert!(
        !world.summarizing(),
        "a turn in flight is not a turn boundary"
    );
}

#[test]
fn a_turn_boundary_is_tried_once_until_the_conversation_grows() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut state = CompactionState::default();
    assert!(state.may_start("ene", 7));
    state.note_attempt("ene", 7);
    let task = background::spawn(runtime.handle(), None, std::future::pending::<Outcome>());
    state.start("ene", task);

    // The call is collected without a new turn: the same boundary again.
    state.companions.get_mut("ene").unwrap().running = None;
    assert!(
        !state.may_start("ene", 7),
        "a standing boundary is not retried"
    );
    assert!(state.may_start("ene", 8), "the next turn is a new boundary");
}

// === One job, one range: the summary and the memories it commits ===

/// `turns` answered turns with a tool call each, the way a real
/// conversation has them, so a summary has something to keep and the
/// memory pass has answered turns to read.
fn answered(turns: usize) -> History {
    let mut history = History::new();
    for turn in 0..turns {
        history.push(Message::user(format!("question {turn} {}", "x".repeat(40))));
        history.open_turn();
        tool_turn(&mut history, &format!("call-{turn}"), "z".repeat(400));
        history.push(Message::assistant(format!(
            "answer {turn} {}",
            "y".repeat(40)
        )));
        history.end_turn(TurnEnd::Answered { answer: None });
    }
    history
}

fn fact() -> serde_json::Value {
    serde_json::json!([{
        "content": "A keeps the tea in the cupboard",
        "importance": 7,
        "scope": "",
        "supersedes": null,
        "cited_memory_ids": []
    }])
}

/// A local OpenAI-compatible endpoint that answers both halves of a job:
/// the extraction call with `output`, every other call with `summary`. The
/// request bodies are kept, so a test can read what each model was asked.
struct Provider {
    base: String,
    bodies: Arc<parking_lot::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(summary: &'static str, output: serde_json::Value) -> Provider {
    serve_with(summary, Some(output)).await
}

/// `output` `None` makes the extraction call fail: the model answers with
/// text instead of the `submit` tool call the extractor requires.
async fn serve_with(summary: &'static str, output: Option<serde_json::Value>) -> Provider {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let body = loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    break None;
                };
                if read == 0 {
                    break None;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buffer[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break Some(String::from_utf8_lossy(&buffer[end + 4..]).to_string());
                }
            };
            let Some(body) = body else {
                return;
            };
            let extraction = body.contains("\"submit\"");
            seen.lock().push(body);
            let answer = match (&output, extraction) {
                (Some(output), true) => tool_call(&output.clone()),
                _ => text(summary),
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                answer.len()
            );
            let _discarded = socket.write_all(head.as_bytes()).await;
            let _discarded = socket.write_all(answer.as_bytes()).await;
            let _discarded = socket.flush().await;
        }
    });
    Provider { base, bodies, task }
}

fn text(text: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-summary",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
    .to_string()
}

fn tool_call(output: &serde_json::Value) -> String {
    serde_json::json!({
        "id": "chatcmpl-extraction",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": { "name": "submit", "arguments": output.to_string() }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
    .to_string()
}

/// One compaction job covers one range. The summary it writes stands for a
/// message window, and the memories it commits come from exactly the turns
/// in that window, stamped with the same window end; the aside is written
/// after the commit, so a job that committed memories has its summary.
#[test]
fn one_job_covers_the_same_range_for_the_summary_and_the_memories() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let provider = runtime.block_on(serve(
        "既存の紅茶の話",
        serde_json::json!({ "facts": fact() }),
    ));
    let history = answered(4);
    // The trigger is over its ratio at 400 tokens of room; the summarizer
    // keeps 300 of the 1,200 the catalog reports, which cuts before the
    // newest turns.
    let mut world = SummaryWorld::at(history.clone(), 400, 1_200, &provider.base);
    assert!(
        world.ratio() >= THRESHOLD_RATIO,
        "the fixture is over the threshold: {}",
        world.ratio()
    );
    world.run();

    let conversation = world.app.world().get::<Conversation>(world.agent);
    let conversation = conversation.expect("the companion's conversation");
    let summary = summary_of(&conversation.0).expect("the summary aside was written");
    assert_eq!(summary.text, "既存の紅茶の話");

    // The turns the summary stands for, and the ones it does not.
    let window: Vec<String> =
        consolidate::pending_turns(&history, None, summary.through_message_id)
            .into_iter()
            .map(|turn| turn.turn_id)
            .collect();
    let all: Vec<String> = consolidate::pending_turns(&history, None, u64::MAX)
        .into_iter()
        .map(|turn| turn.turn_id)
        .collect();
    let outside: Vec<&String> = all.iter().filter(|id| !window.contains(id)).collect();
    assert!(
        !window.is_empty() && !outside.is_empty(),
        "the fixture must leave turns on both sides: {window:?} of {all:?}"
    );

    let bodies = provider.bodies.lock().clone();
    assert_eq!(
        bodies.len(),
        2,
        "one job: one summary call and one extraction call"
    );
    let extraction = bodies
        .iter()
        .find(|body| body.contains("\"submit\""))
        .expect("the pass asked the extraction model");
    for id in &window {
        assert!(
            extraction.contains(&format!("[turn {id}]")),
            "turn {id} is inside the window: {extraction}"
        );
    }
    for id in outside {
        assert!(
            !extraction.contains(&format!("[turn {id}]")),
            "turn {id} is outside the window: {extraction}"
        );
    }

    let store = world.app.world().resource::<Store>();
    let (memories, _) = store.memory_repo().list_memories("ene", None, 50).unwrap();
    assert_eq!(memories.len(), 1, "the job committed its one fact");
    assert_eq!(memories[0].content, "A keeps the tea in the cupboard");
    assert_eq!(
        memories[0].source_context["turn_id"],
        serde_json::json!(summary.through_message_id.to_string()),
        "the memory is stamped with the same end of the window"
    );
    provider.task.abort();
}

/// The aside is the position the next job starts from, so a pass that
/// commits nothing leaves it unwritten: the stored summary stays the one
/// the range is measured from, and the next compaction covers the range
/// again. No queue and no cursor is involved.
#[test]
fn a_failed_pass_leaves_the_summary_unwritten_and_the_range_pending() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // The extraction answers with text instead of the `submit` call, so the
    // pass cannot produce facts.
    let provider = runtime.block_on(serve_with("既存の紅茶の話", None));
    let mut world = SummaryWorld::at(answered(4), 400, 1_200, &provider.base);
    world.run();

    let conversation = world.app.world().get::<Conversation>(world.agent);
    assert!(
        summary_of(&conversation.expect("the companion's conversation").0).is_none(),
        "a failed pass does not advance the stored summary"
    );
    let bodies = provider.bodies.lock().clone();
    assert_eq!(
        bodies.len(),
        2,
        "the summary ran, then the pass asked the extraction model"
    );
    assert!(
        !bodies[0].contains("\"submit\""),
        "the summary call is the first one"
    );
    assert!(
        bodies[1].contains("\"submit\""),
        "the extraction call is the one that failed"
    );
    let store = world.app.world().resource::<Store>();
    let (memories, _) = store.memory_repo().list_memories("ene", None, 50).unwrap();
    assert!(memories.is_empty(), "a failed pass commits nothing");
    provider.task.abort();
}
