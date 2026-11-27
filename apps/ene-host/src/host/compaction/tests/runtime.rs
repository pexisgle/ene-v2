use super::*;

mod provider;

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
    fn at(
        mut conversation: History,
        threshold_window: u32,
        summary_window: u32,
        base: &str,
    ) -> Self {
        conversation.continue_log(
            rig_core::conversation::Lineage::fresh(),
            conversation.revision(),
        );
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
