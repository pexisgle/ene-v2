use super::*;

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
/// in that window, stamped with the same window end. The request view receives
/// the summary after its transaction commits.
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
    assert!(legacy_summary(&conversation.0).is_none());
    let summary = &world
        .app
        .world()
        .get::<CurrentSummary>(world.agent)
        .unwrap()
        .0;
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

/// The checkpoint is the position the next job starts from, so a pass that
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

    assert!(
        world
            .app
            .world()
            .get::<CurrentSummary>(world.agent)
            .is_none(),
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
