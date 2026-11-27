use super::*;

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
