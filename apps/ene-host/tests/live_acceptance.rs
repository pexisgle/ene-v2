//! Live acceptance for the parent conversation: the real host process, the
//! real host API, and a real provider. Ignored by default because it needs
//! credentials from `.env` and makes a small paid call.
//!
//! Run explicitly, single-threaded, with the keys present:
//!
//! ```sh
//! cargo test -p ene-host --test live_acceptance -- --ignored --nocapture
//! ```
//!
//! The test reads the keys itself and registers one through the normal
//! credential API; it never prints the key, an Authorization header or the
//! `.env` contents. Provider selections: `ENE_LIVE_PROVIDER=openai|openrouter`
//! (default openai); `ENE_LIVE_MODEL` overrides the model id.

#![cfg(unix)]

use connectrpc::client::{CallOptions, HttpClient};
use ene_rpc as w;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const CALL: Duration = Duration::from_secs(120);

fn call_options() -> CallOptions {
    CallOptions::default().with_timeout(CALL)
}

/// The variables this test uses, from the repository `.env`; values never
/// leave this map except into the credential RPC.
fn dotenv() -> BTreeMap<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        values.insert(key.trim().to_owned(), value.to_owned());
    }
    values
}

fn host_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ene-host"));
    command
        .env("XDG_RUNTIME_DIR", root)
        .env("ENE_HOST_STATE_DIR", root.join("state"))
        .env("ENE_PLUGIN_DIR", root.join("plugins"))
        .env("ENE_HOST_SESSION_CREDENTIALS", "1")
        .env_remove("ENE_WEB_ORIGIN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

async fn start(root: &Path) -> (tokio::process::Child, w::HostServiceClient<HttpClient>) {
    let mut child = host_command(root).spawn().unwrap();
    let descriptor = root.join("ene-host/connection.json");
    let connection: serde_json::Value = timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "host exited during startup"
            );
            match std::fs::read(&descriptor) {
                Ok(bytes) => break serde_json::from_slice(&bytes).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("connection descriptor: {error}"),
            }
        }
    })
    .await
    .expect("host startup deadline");
    let (transport, config) = w::connect(
        connection["endpoint"].as_str().unwrap(),
        connection["token"].as_str().unwrap(),
    )
    .unwrap();
    (child, w::HostServiceClient::new(transport, config))
}

async fn stop(mut child: tokio::process::Child, root: &Path) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("host stopped")
        .expect("host wait");
    assert!(!root.join("ene-host/connection.json").exists());
}

fn json_doc(text: &str) -> w::JsonDocument {
    w::JsonDocument {
        json: text.into(),
        ..Default::default()
    }
}

struct GrantSetup<'a> {
    key: &'a str,
    companion: &'a str,
    connection: &'a str,
    settings_revision: u64,
    provider_revision: &'a str,
    destinations: &'a [String],
    data_kinds: &'a [&'a str],
    purpose: &'a str,
}

async fn grant(client: &w::HostServiceClient<HttpClient>, setup: GrantSetup<'_>) {
    client
        .create_grant_with_options(
            w::CreateGrantRequest {
                request_key: setup.key.into(),
                companion_id: setup.companion.into(),
                connection_id: setup.connection.into(),
                kind: "data_transfer".into(),
                destinations: setup.destinations.to_vec(),
                data_kinds: setup.data_kinds.iter().map(|kind| (*kind).into()).collect(),
                purpose: setup.purpose.into(),
                consent: w::Consent {
                    settings_revision: setup.settings_revision,
                    provider_revision: setup.provider_revision.into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("grant accepted");
}

struct Turn {
    deltas: Vec<String>,
    created: Vec<String>,
    minds: Vec<w::MindView>,
}

async fn send_turn(
    client: &w::HostServiceClient<HttpClient>,
    companion: &str,
    text: &str,
    key: &str,
) -> Turn {
    let mut events = client
        .subscribe(w::Empty::default())
        .await
        .expect("subscribe");
    let accepted = client
        .send_message_with_options(
            w::SendMessageRequest {
                companion_id: companion.into(),
                text: text.into(),
                request_key: key.into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("send")
        .into_owned();
    println!(
        "[live] accepted key={key} request_id={}",
        accepted.request_id
    );
    let mut turn = Turn {
        deltas: Vec::new(),
        created: Vec::new(),
        minds: Vec::new(),
    };
    timeout(Duration::from_secs(120), async {
        loop {
            let Some(event) = events.message::<w::HostEvent>().await.unwrap() else {
                panic!("subscription ended before the turn ended");
            };
            let event = event.to_owned_message();
            let Some(w::host_event::Event::Notification(notification)) = event.event else {
                continue;
            };
            match notification.event {
                Some(w::notification::Event::MessageDelta(delta)) => {
                    turn.deltas.push(delta.delta);
                }
                Some(w::notification::Event::MessageCreated(message)) => {
                    turn.created.push(message.text);
                }
                Some(w::notification::Event::MindUpdated(view)) => {
                    turn.minds.push(*view);
                }
                Some(w::notification::Event::TurnEvent(event)) => match event.state.as_str() {
                    "answered" => return,
                    "stopped" | "failed" => panic!("turn ended {}", event.state),
                    _ => {}
                },
                _ => {}
            }
        }
    })
    .await
    .expect("turn deadline");
    turn
}

#[derive(Debug)]
struct ToolExchange {
    name: String,
    call_id: String,
    arguments: serde_json::Value,
    result: Option<ToolResultRecord>,
}

#[derive(Debug)]
struct ToolResultRecord {
    name: String,
    text: String,
    is_error: bool,
}

struct LogFacts {
    tools: Vec<ToolExchange>,
    final_answer: Option<String>,
    system_prompts: Vec<String>,
    usage: Vec<(i64, i64, f64)>,
}

/// The call id as the log writes it: provider-issued or rig-local.
fn call_id(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    for kind in ["provider", "local"] {
        if let Some(id) = object
            .get(kind)
            .and_then(|id| id.as_str())
            .filter(|id| !id.is_empty())
        {
            return Some(format!("{kind}:{id}"));
        }
    }
    None
}

/// Text blocks of a message content array.
fn content_text(content: &serde_json::Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn log_facts(state: &Path, companion: &str) -> LogFacts {
    fn find_usage(value: &serde_json::Value, found: &mut Vec<(i64, i64, f64)>) {
        match value {
            serde_json::Value::Object(object) => {
                let input = object.get("input_tokens").and_then(|value| value.as_i64());
                let output = object.get("output_tokens").and_then(|value| value.as_i64());
                if let (Some(input), Some(output)) = (input, output) {
                    let cost = object
                        .get("cost")
                        .and_then(|cost| cost.get("total"))
                        .and_then(|cost| cost.as_f64())
                        .unwrap_or_default();
                    found.push((input, output, cost));
                    return;
                }
                for value in object.values() {
                    find_usage(value, found);
                }
            }
            serde_json::Value::Array(items) => {
                for value in items {
                    find_usage(value, found);
                }
            }
            _ => {}
        }
    }
    let path = state.join("rig/logs").join(format!("{companion}.jsonl"));
    let text = std::fs::read_to_string(&path).expect("Rig session log");
    let mut tools: Vec<ToolExchange> = Vec::new();
    let mut results: Vec<(String, ToolResultRecord)> = Vec::new();
    let mut final_answer = None;
    let mut system_prompts = Vec::new();
    let mut usage = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        find_usage(&value, &mut usage);
        if value["type"] == "state"
            && let Some(text) = value["value"]["content"].as_str()
        {
            system_prompts.push(text.to_owned());
        }
        let messages = value["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| &entry["message"])
            .chain(value["with"].as_array().into_iter().flatten())
            .chain(value["state"]["messages"].as_array().into_iter().flatten());
        for message in messages {
            match message["role"].as_str() {
                Some("system") => {
                    if let Some(content) = message["content"].as_str() {
                        system_prompts.push(content.to_owned());
                    }
                }
                Some("assistant") => {
                    let Some(content) = message["content"].as_array() else {
                        continue;
                    };
                    for item in content {
                        match item["type"].as_str() {
                            Some("toolcall") => {
                                let Some(call_id) = call_id(&item["id"]) else {
                                    continue;
                                };
                                let Some(name) = item["function"]["name"].as_str() else {
                                    continue;
                                };
                                tools.push(ToolExchange {
                                    name: name.into(),
                                    call_id,
                                    arguments: item["function"]["arguments"].clone(),
                                    result: None,
                                });
                            }
                            Some("text") => {
                                if let Some(answer) = item["text"].as_str() {
                                    final_answer = Some(answer.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some("user") => {
                    let Some(content) = message["content"].as_array() else {
                        continue;
                    };
                    for item in content {
                        if item["type"] != "toolresult" {
                            continue;
                        }
                        let Some(call_id) = call_id(&item["call"]) else {
                            continue;
                        };
                        let name = item["name"].as_str().unwrap_or_default().to_owned();
                        results.push((
                            call_id,
                            ToolResultRecord {
                                name,
                                text: content_text(&item["content"]),
                                is_error: item["is_error"].as_bool().unwrap_or(false),
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    for tool in &mut tools {
        tool.result = results
            .iter()
            .find(|(call_id, result)| call_id == &tool.call_id && result.name == tool.name)
            .map(|(_, result)| ToolResultRecord {
                name: result.name.clone(),
                text: result.text.clone(),
                is_error: result.is_error,
            });
    }
    LogFacts {
        tools,
        final_answer,
        system_prompts,
        usage,
    }
}

async fn log_with_turn(state: &Path, companion: &str) -> LogFacts {
    for _ in 0..40 {
        let facts = log_facts(state, companion);
        if facts.final_answer.is_some() && facts.tools.iter().any(|tool| tool.result.is_some()) {
            return facts;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    log_facts(state, companion)
}

fn timestamp_in(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for index in 0..bytes.len().saturating_sub(19) {
        let window = &bytes[index..index + 19];
        let digit = |at: usize| window[at].is_ascii_digit();
        if digit(0)
            && digit(1)
            && digit(2)
            && digit(3)
            && window[4] == b'-'
            && digit(5)
            && digit(6)
            && window[7] == b'-'
            && digit(8)
            && digit(9)
            && window[10] == b'T'
            && digit(11)
            && digit(12)
            && window[13] == b':'
            && digit(14)
            && digit(15)
            && window[16] == b':'
            && digit(17)
            && digit(18)
        {
            return Some(String::from_utf8_lossy(window).into_owned());
        }
    }
    None
}

fn summarize(usage: &[(i64, i64, f64)]) -> (i64, i64, f64) {
    (
        usage.iter().map(|(input, _, _)| input).sum(),
        usage.iter().map(|(_, output, _)| output).sum(),
        usage.iter().map(|(_, _, cost)| cost).sum(),
    )
}

fn choose_model(vendor: &str, available: &[String], preferred: Option<&str>) -> String {
    let catalog = rig_core::catalog::Catalog::builtin();
    let tool_model = |id: &str| {
        catalog
            .resolve(&format!("{vendor}/{id}"))
            .ok()
            .filter(|resolved| resolved.spec.tools)
            .map(|resolved| resolved.spec)
    };
    if let Some(preferred) = preferred
        && available.iter().any(|id| id == preferred)
        && tool_model(preferred).is_some()
    {
        return preferred.to_owned();
    }
    let mut candidates = available
        .iter()
        .filter_map(|id| tool_model(id).map(|spec| (id.clone(), spec)))
        .collect::<Vec<_>>();
    candidates.sort_by(|(_, a), (_, b)| {
        let price = |spec: &rig_core::catalog::ModelSpec| {
            spec.pricing
                .as_ref()
                .map(|pricing| pricing.input + pricing.output)
                .unwrap_or(f64::MAX)
        };
        price(a).total_cmp(&price(b))
    });
    let chosen = candidates
        .first()
        .unwrap_or_else(|| panic!("no tool-capable model among {} offered", available.len()));
    let price = chosen
        .1
        .pricing
        .as_ref()
        .map(|pricing| pricing.input + pricing.output);
    println!(
        "[live] chose {}/{} (USD/M input+output: {:?})",
        vendor, chosen.0, price
    );
    chosen.0.clone()
}

/// The full live scenario. Ignored because it spends money and needs keys.
#[tokio::test]
#[ignore = "live provider call; needs .env keys and spends a few cents"]
async fn provider_acceptance_through_the_host() {
    let provider = std::env::var("ENE_LIVE_PROVIDER").unwrap_or_else(|_| "openai".into());
    let keys = dotenv();
    let (env_name, adapter, endpoint) = match provider.as_str() {
        "openai" => ("OPENAI_API_KEY", "openai", "https://api.openai.com/v1"),
        "openrouter" => (
            "OPENROUTER_API_KEY",
            "openrouter",
            "https://openrouter.ai/api/v1",
        ),
        other => panic!("unknown provider {other}"),
    };
    let Some(key) = keys.get(env_name).filter(|key| !key.is_empty()) else {
        eprintln!("[live] {env_name} is not present in .env; skipped");
        return;
    };

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::set_permissions(
        &root,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let state = root.join("state");
    if std::env::var("ENE_LIVE_KEEP").as_deref() == Ok("1") {
        println!("[live] kept root {}", root.display());
        std::mem::forget(temp);
    }
    let (child, client) = start(&root).await;

    let snapshot = client
        .get_snapshot_with_options(w::SnapshotRequest::default(), call_options())
        .await
        .expect("snapshot")
        .into_owned();
    let companion = snapshot.companions[0].clone();
    let provider_info = client
        .list_providers_with_options(w::Empty::default(), call_options())
        .await
        .expect("providers")
        .into_owned()
        .items
        .into_iter()
        .find(|provider| provider.id == adapter)
        .unwrap_or_else(|| panic!("{adapter} descriptor"));
    let destinations = provider_info.destinations.clone();
    let provider_revision = provider_info.revision.clone();

    let settings = client
        .update_settings_with_options(
            w::UpdateSettingsRequest {
                revision: snapshot
                    .settings
                    .as_option()
                    .map(|s| s.revision)
                    .unwrap_or(1),
                connections: w::ConnectionInputs {
                    items: vec![w::ConnectionInput {
                        kind: "llm".into(),
                        adapter: adapter.into(),
                        endpoint: endpoint.into(),
                        options: json_doc("{}").into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("settings")
        .into_owned();
    let connection = settings.connections[0].id.clone();
    let credential = client
        .register_credentials_with_options(
            w::CredentialRequest {
                id: connection.clone(),
                secret: Some(key.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("credentials")
        .into_owned();
    println!(
        "[live] provider={provider} connection={adapter} credential_state={}",
        credential.credential_state
    );

    let settings_revision = settings.revision;
    let mut all_destinations = destinations.clone();
    all_destinations.sort();
    all_destinations.dedup();
    grant(
        &client,
        GrantSetup {
            key: "live-conversation",
            companion: &companion.id,
            connection: &connection,
            settings_revision,
            provider_revision: &provider_revision,
            destinations: &all_destinations,
            data_kinds: &["conversation"],
            purpose: "conversation",
        },
    )
    .await;
    grant(
        &client,
        GrantSetup {
            key: "live-metadata",
            companion: &companion.id,
            connection: &connection,
            settings_revision,
            provider_revision: &provider_revision,
            destinations: &all_destinations,
            data_kinds: &["metadata", "test_input"],
            purpose: "connection_check",
        },
    )
    .await;

    let models = client
        .list_models_with_options(
            w::ModelsRequest {
                id: connection.clone(),
                companion_id: Some(companion.id.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("model list")
        .into_owned();
    let available = models
        .items
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    let chosen = match std::env::var("ENE_LIVE_MODEL") {
        Ok(model) if !model.is_empty() => model,
        _ => choose_model(
            adapter,
            &available,
            (provider == "openai").then_some("gpt-6-luna"),
        ),
    };
    println!("[live] models_offered={} chosen={chosen}", available.len());

    // OpenRouter has no one-shot check path yet; it must say so without
    // sending a model request (the OpenAI check is covered by unit tests to
    // avoid another paid call here).
    if provider == "openrouter" {
        let error = client
            .check_connection_with_options(
                w::CheckRequest {
                    id: connection.clone(),
                    request_key: "live-check".into(),
                    companion_id: companion.id.clone(),
                    purpose: "companion".into(),
                    model_id: Some(chosen.clone()),
                    ..Default::default()
                },
                call_options(),
            )
            .await
            .expect_err("OpenRouter check is not implemented");
        println!("[live] check_connection unsupported code={:?}", error.code);
        assert_eq!(error.code, connectrpc::ErrorCode::InvalidArgument);
    }

    let _updated = client
        .update_companion_with_options(
            w::UpdateCompanionRequest {
                id: companion.id.clone(),
                revision: companion.revision,
                mind: Some("日本語で簡潔に答える。ツールの結果をそのまま使う。".into()),
                companion_assignment: w::AssignmentPatch {
                    change: Some(w::assignment_patch::Change::Set(Box::new(
                        w::ModelAssignment {
                            connection_id: connection.clone(),
                            model_id: chosen.clone(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("companion")
        .into_owned();

    let first_prompt = "現在時刻を current_time ツールで1回だけ確認し、ツールが返した時刻だけを短く答えてください。";
    let first = send_turn(&client, &companion.id, first_prompt, "live-1").await;
    println!(
        "[live] turn1 deltas={} messages={:?}",
        first.deltas.len(),
        first.created
    );

    let facts = log_with_turn(&state, &companion.id).await;
    let (input_tokens, output_tokens, cost) = summarize(&facts.usage);
    let exchange = facts
        .tools
        .iter()
        .find(|tool| tool.name == "current_time")
        .expect("the assistant requested no current_time tool call");
    assert!(!exchange.call_id.is_empty(), "the tool call had no id");
    assert!(
        exchange
            .arguments
            .as_object()
            .is_some_and(|args| args.is_empty()),
        "current_time received unexpected arguments: {:?}",
        exchange.arguments
    );
    let result = exchange
        .result
        .as_ref()
        .filter(|result| !result.is_error)
        .expect("no successful result matched the tool call id");
    let tool_time = timestamp_in(&result.text).expect("the tool result carried no timestamp");
    let final_answer = facts
        .final_answer
        .as_deref()
        .expect("the log has no final assistant text");
    println!(
        "[live] tool name={} call_id={} result={} answer_matches={} calls_with_usage={} input_tokens={input_tokens} output_tokens={output_tokens} catalog_cost_usd={cost:.8}",
        exchange.name,
        exchange.call_id,
        tool_time,
        final_answer.contains(&tool_time),
        facts.usage.len()
    );

    let history = client
        .get_history_with_options(
            w::HistoryRequest {
                companion_id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("history")
        .into_owned();
    assert_eq!(history.items.len(), 2, "user and assistant messages");
    assert_eq!(history.items[0].role, "user");
    assert_eq!(history.items[1].role, "assistant");
    assert_eq!(
        facts.final_answer.as_deref(),
        Some(history.items[1].text.as_str()),
        "the Rig log's final assistant text is not the confirmed answer"
    );
    assert!(
        history.items[1].text.contains(&tool_time),
        "the confirmed answer did not use the matched tool result"
    );
    println!("[live] first answer: {}", history.items[1].text);

    let second = send_turn(
        &client,
        &companion.id,
        "直前の返答で示した時刻を、そのまま1回だけ繰り返してください。",
        "live-2",
    )
    .await;
    println!("[live] turn2 deltas={}", second.deltas.len());
    let history = client
        .get_history_with_options(
            w::HistoryRequest {
                companion_id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("history after follow-up")
        .into_owned();
    assert_eq!(history.items.len(), 4, "the follow-up keeps the first turn");
    assert_eq!(history.items[2].role, "user");
    assert_eq!(history.items[3].role, "assistant");
    println!("[live] follow-up answer: {}", history.items[3].text);
    let facts = log_with_turn(&state, &companion.id).await;
    let (input_tokens, output_tokens, cost) = summarize(&facts.usage);
    println!(
        "[live] after follow-up tool_calls={} calls_with_usage={} total_input_tokens={input_tokens} total_output_tokens={output_tokens} catalog_cost_usd={cost:.8}",
        facts.tools.len(),
        facts.usage.len()
    );

    stop(child, &root).await;
    let (_child, restarted) = start(&root).await;
    let history = restarted
        .get_history_with_options(
            w::HistoryRequest {
                companion_id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("history after restart")
        .into_owned();
    assert_eq!(history.items.len(), 4, "history survives a restart");
    assert_eq!(history.items[0].text, first_prompt);
    println!("[live] restart: {} messages restored", history.items.len());
}

async fn db_mind(path: &Path) -> (Vec<serde_json::Value>, serde_json::Value) {
    let db = tokio_rusqlite::Connection::open(path).await.unwrap();
    let (rows, state) = db
        .call(|db| -> std::result::Result<(Vec<serde_json::Value>, String), tokio_rusqlite::rusqlite::Error> {
            let mut statement = db.prepare(
                "SELECT content,sources,supersedes,acquisition,recorded_at FROM mind_memories ORDER BY recorded_at,id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(serde_json::json!({
                        "content": row.get::<_, String>(0)?,
                        "sources": row.get::<_, String>(1)?,
                        "supersedes": row.get::<_, String>(2)?,
                        "acquisition": row.get::<_, String>(3)?,
                        "recorded_at": row.get::<_, String>(4)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let state: String =
                db.query_row("SELECT state FROM mind_companions LIMIT 1", [], |row| row.get(0))?;
            Ok((rows, state))
        })
        .await
        .unwrap();
    db.close().await.unwrap();
    (rows, serde_json::from_str(&state).unwrap())
}

async fn copy_mind_database(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let db = tokio_rusqlite::Connection::open(source).await.unwrap();
    let destination = destination.to_path_buf();
    let target = destination.clone();
    db.call(
        move |db| -> std::result::Result<(), tokio_rusqlite::rusqlite::Error> {
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            db.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
            Ok(())
        },
    )
    .await
    .unwrap();
    db.close().await.unwrap();
    std::fs::set_permissions(
        &destination,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .unwrap();
    std::fs::set_permissions(
        destination.parent().unwrap(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
}

fn newer_root() -> PathBuf {
    let path = tempfile::tempdir().unwrap().keep();
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    path
}

async fn mind_update_result(facts: &LogFacts) -> Option<serde_json::Value> {
    facts
        .tools
        .iter()
        .rev()
        .find(|tool| tool.name == "mind_update")
        .and_then(|tool| tool.result.as_ref())
        .filter(|result| !result.is_error)
        .and_then(|result| serde_json::from_str(&result.text).ok())
}

async fn recall_result(facts: &LogFacts) -> Option<serde_json::Value> {
    facts
        .tools
        .iter()
        .find(|tool| tool.name == "mind_recall")
        .and_then(|tool| tool.result.as_ref())
        .filter(|result| !result.is_error)
        .and_then(|result| serde_json::from_str(&result.text).ok())
}

/// The step 2 live path: an anonymous character, a synthetic preference, a
/// real mind update, a correction, a fresh host with no conversation (memory
/// must be read), and a model change to OpenRouter. Ignored: spends a few
/// cents and needs .env keys.
#[tokio::test]
#[ignore = "live mind acceptance; needs .env keys and spends a few cents"]
async fn mind_acceptance_through_the_host() {
    let keys = dotenv();
    let Some(openai_key) = keys.get("OPENAI_API_KEY").filter(|key| !key.is_empty()) else {
        panic!("OPENAI_API_KEY is missing; the mind acceptance cannot be verified without it");
    };
    let Some(openrouter_key) = keys.get("OPENROUTER_API_KEY").filter(|key| !key.is_empty()) else {
        panic!("OPENROUTER_API_KEY is missing; the compatibility check cannot run");
    };

    let root = newer_root();
    let state = root.join("state");
    let (child, client) = start(&root).await;
    let snapshot = client
        .get_snapshot_with_options(w::SnapshotRequest::default(), call_options())
        .await
        .expect("snapshot")
        .into_owned();
    let companion = snapshot.companions[0].clone();
    let providers = client
        .list_providers_with_options(w::Empty::default(), call_options())
        .await
        .expect("providers")
        .into_owned();
    let openai = providers
        .items
        .iter()
        .find(|provider| provider.id == "openai")
        .expect("openai descriptor");
    let settings = client
        .update_settings_with_options(
            w::UpdateSettingsRequest {
                revision: snapshot
                    .settings
                    .as_option()
                    .map(|s| s.revision)
                    .unwrap_or(1),
                connections: w::ConnectionInputs {
                    items: vec![w::ConnectionInput {
                        kind: "llm".into(),
                        adapter: "openai".into(),
                        endpoint: "https://api.openai.com/v1".into(),
                        options: json_doc("{}").into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("settings")
        .into_owned();
    let openai_connection = settings.connections[0].id.clone();
    let settings_revision = settings.revision;
    client
        .register_credentials_with_options(
            w::CredentialRequest {
                id: openai_connection.clone(),
                secret: Some(openai_key.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("openai credential");
    let destinations = openai.destinations.clone();
    grant(
        &client,
        GrantSetup {
            key: "mind-openai-conversation",
            companion: &companion.id,
            connection: &openai_connection,
            settings_revision,
            provider_revision: &openai.revision,
            destinations: &destinations,
            data_kinds: &["conversation"],
            purpose: "conversation",
        },
    )
    .await;
    client
        .update_companion_with_options(
            w::UpdateCompanionRequest {
                id: companion.id.clone(),
                revision: companion.revision,
                mind: Some(
                    "あなたは架空の静かな助手「ミナ」。落ち着いた口調で、相手の話をよく聞く。"
                        .into(),
                ),
                companion_assignment: w::AssignmentPatch {
                    change: Some(w::assignment_patch::Change::Set(Box::new(
                        w::ModelAssignment {
                            connection_id: openai_connection.clone(),
                            model_id: "gpt-6-luna".into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("companion");
    let profile = client
        .update_mind_profile_with_options(
            w::UpdateMindProfileRequest {
                companion_id: companion.id.clone(),
                expected_revision: 1,
                facets: vec!["静かだが、相手の好みをよく覚えている".into()],
                examples: vec!["「なるほど」と短く返す".into()],
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("profile")
        .into_owned();
    assert_eq!(profile.profile.as_option().map(|p| p.revision), Some(2));

    // 1. A synthetic experience and explicit mind update.
    let first = send_turn(
        &client,
        &companion.id,
        "私は柚子が好きです。この好みを mind_update で記憶し、現在の心も「少しうれしい」と短く更新してください。現在の心には果物の名前を書かないでください。作者の会話例は実体験として記憶しないでください。",
        "mind-first",
    )
    .await;
    let facts = log_with_turn(&state, &companion.id).await;
    let first_update = mind_update_result(&facts)
        .await
        .expect("the model did not request mind_update");
    let old_id = first_update["memories"][0]["id"]
        .as_str()
        .expect("mind_update returned no memory id")
        .to_owned();
    let first_revision = first_update["revision"].as_u64().unwrap_or_default();
    let (first_input, first_output, first_cost) = summarize(&facts.usage);
    println!(
        "[mind] first update revision={first_revision} memory={old_id} minds_notified={} calls_with_usage={} input_tokens={first_input} output_tokens={first_output} catalog_cost_usd={first_cost:.8}",
        first.minds.len(),
        facts.usage.len()
    );
    let seen_usage = facts.usage.len();
    assert!(first.minds.iter().any(|view| {
        view.state
            .as_option()
            .is_some_and(|state| state.revision == first_revision)
    }));
    let mind = client
        .get_mind_with_options(
            w::Id {
                id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("get mind")
        .into_owned();
    assert!(
        mind.state
            .as_option()
            .is_some_and(|state| state.revision >= 2)
    );
    assert!(
        mind.profile
            .as_option()
            .is_some_and(|profile| profile.examples == ["「なるほど」と短く返す"])
    );
    let (rows, state_json) = db_mind(&state.join("host.sqlite3")).await;
    let contents = rows
        .iter()
        .filter_map(|row| row["content"].as_str())
        .collect::<Vec<_>>();
    assert!(contents.iter().any(|content| content.contains("柚子")));
    assert!(
        contents.iter().all(|content| !content.contains("なるほど")),
        "an author example was stored as experience"
    );
    let state_text = state_json.to_string();
    assert!(
        !state_text.contains("柚子"),
        "memory content leaked into current mind"
    );
    let source =
        serde_json::from_str::<Vec<serde_json::Value>>(rows[0]["sources"].as_str().unwrap())
            .unwrap();
    assert_eq!(source[0]["kind"], "input");
    assert!(!source[0]["id"].as_str().unwrap().is_empty());

    // 2. Correct it, keeping the old memory and its reference.
    let correction = format!(
        "さっきの好みは間違いでした。本当はりんごが好きです。mind_update で訂正し、supersedes に {old_id} を入れてください。現在の心には果物の名前を書かないでください。"
    );
    let corrected_turn = send_turn(&client, &companion.id, &correction, "mind-correction").await;
    let correction_facts = log_with_turn(&state, &companion.id).await;
    let correction_update = mind_update_result(&correction_facts)
        .await
        .expect("the model did not correct the memory");
    let new_id = correction_update["memories"][0]["id"]
        .as_str()
        .expect("correction returned no id")
        .to_owned();
    let correction_revision = correction_update["revision"].as_u64().unwrap_or_default();
    let correction_usage = &correction_facts.usage[seen_usage..];
    let correction_calls = correction_usage.len();
    let (correction_input, correction_output, correction_cost) = summarize(correction_usage);
    println!(
        "[mind] correction revision={correction_revision} memory={new_id} minds_notified={} input_tokens={correction_input} output_tokens={correction_output} catalog_cost_usd={correction_cost:.8}",
        corrected_turn.minds.len()
    );
    let (rows, state_json) = db_mind(&state.join("host.sqlite3")).await;
    assert!(rows.iter().any(|row| {
        row["content"]
            .as_str()
            .is_some_and(|content| content.contains("柚子"))
    }));
    let new_row = rows
        .iter()
        .find(|row| {
            row["supersedes"]
                .as_str()
                .is_some_and(|s| s.contains(&old_id))
        })
        .expect("the correction kept no supersedes reference");
    assert!(new_row["content"].as_str().unwrap().contains("りんご"));
    assert!(!state_json.to_string().contains("りんご"));
    assert!(!state_json.to_string().contains("柚子"));

    // 3. A fresh host with no conversation: the specific fact is only in the
    // saved memory and must be read with mind_recall.
    stop(child, &root).await;
    let root2 = newer_root();
    std::fs::create_dir_all(root2.join("state")).unwrap();
    copy_mind_database(
        &state.join("host.sqlite3"),
        &root2.join("state/host.sqlite3"),
    )
    .await;
    let state2 = root2.join("state");
    let (child2, client2) = start(&root2).await;
    let _snapshot2 = client2
        .get_snapshot_with_options(w::SnapshotRequest::default(), call_options())
        .await
        .expect("snapshot after restart")
        .into_owned();
    client2
        .register_credentials_with_options(
            w::CredentialRequest {
                id: openai_connection.clone(),
                secret: Some(openai_key.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("credential after restart");
    let recall_prompt = "以前に私が話した好きな果物を思い出してください。会話履歴には残っていないので、必ず mind_recall ツールを、query を「好き」にして呼び、保存済みの記憶から現在の認識を答えてください。";
    let recall_turn = send_turn(&client2, &companion.id, recall_prompt, "mind-recall").await;
    let recall_facts = log_with_turn(&state2, &companion.id).await;
    let recalled = recall_result(&recall_facts)
        .await
        .expect("the fresh conversation did not use mind_recall");
    let recalled_text = recalled.to_string();
    let recall_call = recall_facts
        .tools
        .iter()
        .find(|tool| tool.name == "mind_recall")
        .expect("mind_recall exchange");
    println!(
        "[mind] recall args={} result={recalled_text}",
        recall_call.arguments
    );
    assert!(
        recalled_text.contains("りんご"),
        "the recalled memory did not carry the corrected fact: {recalled_text}"
    );
    assert!(
        recalled_text.contains(&old_id),
        "the correction link was not visible to the model: {recalled_text}"
    );
    let recall_answer = recall_turn.created.last().cloned().unwrap_or_default();
    let (recall_input, recall_output, recall_cost) = summarize(&recall_facts.usage);
    println!(
        "[mind] fresh recall answer: {recall_answer} input_tokens={recall_input} output_tokens={recall_output} catalog_cost_usd={recall_cost:.8}"
    );
    assert!(recall_answer.contains("りんご"), "{recall_answer}");
    let raw = std::fs::read_to_string(
        state2
            .join("rig/logs")
            .join(format!("{}.jsonl", companion.id)),
    )
    .unwrap();
    assert!(
        !raw.contains("この好みを mind_update で記憶し"),
        "the fresh host still had the original conversation"
    );
    assert!(
        recall_facts
            .system_prompts
            .iter()
            .all(|prompt| !prompt.contains("柚子") && !prompt.contains("りんご")),
        "the current mind prompt contained the fact before recall: {:?}",
        recall_facts.system_prompts
    );
    let mind_after = client2
        .get_mind_with_options(
            w::Id {
                id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("mind after restart")
        .into_owned();
    assert_eq!(
        mind_after.state.as_option().map(|state| state.revision),
        Some(correction_revision)
    );

    // 4. Change the conversation model to OpenRouter; the same mind must be
    // read through the changed tool definitions and result format.
    stop(child2, &root2).await;
    let root3 = newer_root();
    std::fs::create_dir_all(root3.join("state")).unwrap();
    copy_mind_database(
        &state2.join("host.sqlite3"),
        &root3.join("state/host.sqlite3"),
    )
    .await;
    let state3 = root3.join("state");
    let (child3, client3) = start(&root3).await;
    let snapshot3 = client3
        .get_snapshot_with_options(w::SnapshotRequest::default(), call_options())
        .await
        .expect("snapshot for model change")
        .into_owned();
    let providers3 = client3
        .list_providers_with_options(w::Empty::default(), call_options())
        .await
        .expect("providers for model change")
        .into_owned();
    let openrouter = providers3
        .items
        .iter()
        .find(|provider| provider.id == "openrouter")
        .expect("openrouter descriptor");
    let current_settings = snapshot3.settings.as_option().cloned().unwrap_or_default();
    let existing = current_settings.connections[0].clone();
    let changed = client3
        .update_settings_with_options(
            w::UpdateSettingsRequest {
                revision: current_settings.revision,
                connections: w::ConnectionInputs {
                    items: vec![
                        w::ConnectionInput {
                            id: Some(existing.id.clone()),
                            kind: existing.kind.clone(),
                            adapter: existing.adapter.clone(),
                            endpoint: existing.endpoint.clone(),
                            options: existing.options.clone(),
                            ..Default::default()
                        },
                        w::ConnectionInput {
                            kind: "llm".into(),
                            adapter: "openrouter".into(),
                            endpoint: "https://openrouter.ai/api/v1".into(),
                            options: json_doc("{}").into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("openrouter settings")
        .into_owned();
    let openrouter_connection = changed
        .connections
        .iter()
        .find(|connection| connection.adapter == "openrouter")
        .expect("openrouter connection")
        .id
        .clone();
    client3
        .register_credentials_with_options(
            w::CredentialRequest {
                id: openrouter_connection.clone(),
                secret: Some(openrouter_key.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("openrouter credential");
    grant(
        &client3,
        GrantSetup {
            key: "mind-openrouter-conversation",
            companion: &companion.id,
            connection: &openrouter_connection,
            settings_revision: changed.revision,
            provider_revision: &openrouter.revision,
            destinations: &openrouter.destinations,
            data_kinds: &["conversation"],
            purpose: "conversation",
        },
    )
    .await;
    let current_companion = snapshot3.companions[0].clone();
    client3
        .update_companion_with_options(
            w::UpdateCompanionRequest {
                id: companion.id.clone(),
                revision: current_companion.revision,
                companion_assignment: w::AssignmentPatch {
                    change: Some(w::assignment_patch::Change::Set(Box::new(
                        w::ModelAssignment {
                            connection_id: openrouter_connection.clone(),
                            model_id: "google/gemini-2.5-flash-lite".into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("openrouter assignment");
    client3
        .register_credentials_with_options(
            w::CredentialRequest {
                id: openrouter_connection.clone(),
                secret: Some(openrouter_key.clone()),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("openrouter credential after assignment");
    let cross_prompt = "以前に私が話した好きな果物を思い出してください。会話履歴には残っていません。必ず mind_recall ツールを、query を「好き」にして呼び、その結果だけから現在の認識を答えてください。";
    let cross_turn = send_turn(&client3, &companion.id, cross_prompt, "mind-openrouter").await;
    let cross_state = state3.clone();
    let cross_facts = log_with_turn(&cross_state, &companion.id).await;
    println!(
        "[mind] openrouter tools={:?} answer={:?}",
        cross_facts
            .tools
            .iter()
            .map(|tool| (
                &tool.name,
                &tool.arguments,
                tool.result.as_ref().map(|r| &r.text)
            ))
            .collect::<Vec<_>>(),
        cross_turn.created.last()
    );
    let cross_recall = recall_result(&cross_facts)
        .await
        .expect("the OpenRouter model did not use mind_recall");
    assert!(cross_recall.to_string().contains("りんご"));
    let cross_answer = cross_turn.created.last().cloned().unwrap_or_default();
    assert!(cross_answer.contains("りんご"), "{cross_answer}");
    let (cross_rows, _) = db_mind(&state3.join("host.sqlite3")).await;
    assert_eq!(
        cross_rows.len(),
        rows.len(),
        "the model change reset the memories"
    );
    let cross_mind = client3
        .get_mind_with_options(
            w::Id {
                id: companion.id.clone(),
                ..Default::default()
            },
            call_options(),
        )
        .await
        .expect("mind after model change")
        .into_owned();
    assert_eq!(
        cross_mind.state.as_option().map(|state| state.revision),
        Some(correction_revision)
    );
    let (cross_input, cross_output, cross_cost) = summarize(&cross_facts.usage);
    println!(
        "[mind] openrouter answer: {cross_answer} calls_with_usage={} input_tokens={cross_input} output_tokens={cross_output} catalog_cost_usd={cross_cost:.8}",
        cross_facts.usage.len()
    );
    println!(
        "[mind] calls first={} correction={correction_calls} recall={} openrouter={} total={} input_tokens={} output_tokens={} catalog_cost_usd={:.8}",
        facts.usage.len(),
        recall_facts.usage.len(),
        cross_facts.usage.len(),
        facts.usage.len() + correction_calls + recall_facts.usage.len() + cross_facts.usage.len(),
        first_input + correction_input + recall_input + cross_input,
        first_output + correction_output + recall_output + cross_output,
        first_cost + correction_cost + recall_cost + cross_cost
    );

    stop(child3, &root3).await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
    let _ = std::fs::remove_dir_all(&root3);
}
