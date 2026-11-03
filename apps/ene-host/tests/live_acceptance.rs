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
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
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

struct LogFacts {
    tool_calls: usize,
    tool_results: usize,
    times: Vec<String>,
    usage: Vec<(i64, i64, f64)>,
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
    let tool_calls = text.matches("\"current_time\"").count();
    let tool_results = text.matches("\"type\":\"toolresult\"").count();
    let mut times = Vec::new();
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
            times.push(String::from_utf8_lossy(window).into_owned());
        }
    }
    let mut usage = Vec::new();
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            find_usage(&value, &mut usage);
        }
    }
    LogFacts {
        tool_calls,
        tool_results,
        times,
        usage,
    }
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
            data_kinds: &["metadata"],
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

    let facts = log_facts(&state, &companion.id);
    let (input_tokens, output_tokens, cost) = summarize(&facts.usage);
    println!(
        "[live] log current_time_mentions={} tool_results={} times={:?} calls_with_usage={} input_tokens={input_tokens} output_tokens={output_tokens} catalog_cost_usd={cost:.8}",
        facts.tool_calls,
        facts.tool_results,
        facts.times.iter().take(4).collect::<Vec<_>>(),
        facts.usage.len()
    );
    assert!(
        facts.tool_calls > 0,
        "the model did not ask for current_time"
    );
    assert!(
        facts.tool_results > 0,
        "the tool result was not written to the conversation"
    );
    assert!(
        !facts.times.is_empty(),
        "the tool result carried no timestamp"
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
    let facts = log_facts(&state, &companion.id);
    let (input_tokens, output_tokens, cost) = summarize(&facts.usage);
    println!(
        "[live] after follow-up current_time_mentions={} calls_with_usage={} total_input_tokens={input_tokens} total_output_tokens={output_tokens} catalog_cost_usd={cost:.8}",
        facts.tool_calls,
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
    if std::env::var("ENE_LIVE_KEEP").as_deref() == Ok("1") {
        let path = temp.keep();
        println!("[live] kept state at {}", path.display());
    }
}
