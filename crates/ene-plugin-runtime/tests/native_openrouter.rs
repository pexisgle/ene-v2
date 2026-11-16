use base64::{Engine as _, engine::general_purpose::STANDARD};
use ene_plugin::*;
use ene_plugin_runtime::{
    Package,
    instance::{Instance, Services},
    limits::MAX_HTTP_CHUNK,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn package() -> (Package, tempfile::TempDir) {
    let asset = PathBuf::from(
        std::env::var_os("ENE_OPENROUTER_WASM")
            .expect("ENE_OPENROUTER_WASM must point to a freshly built OpenRouter Wasm guest"),
    );
    assert!(asset.is_absolute(), "ENE_OPENROUTER_WASM must be absolute");
    let code = std::fs::read(&asset).expect("read ENE_OPENROUTER_WASM");
    let temp = tempfile::tempdir().unwrap();
    write_private(&temp.path().join("provider.wasm"), code);
    write_private(
        &temp.path().join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION,
            "id": "native-openrouter", "name": "Native OpenRouter", "version": "1",
            "entrypoints": [{
                "id": "llm", "target": "host", "file": "provider.wasm",
                "provides": ["llm"], "uses": ["http"],
                "provider": {
                    "id": "native-openrouter", "endpoint": ENDPOINT,
                    "auth": {"type": "none"}, "destinations": [ENDPOINT]
                }
            }]
        })
        .to_string(),
    );
    (Package::load(temp.path()).unwrap(), temp)
}

struct FixtureHttp {
    body: Vec<u8>,
    read: Mutex<usize>,
    request: Mutex<Option<HttpRequest>>,
}

impl Services for FixtureHttp {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }

    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            let chunk = match request {
                ServiceRequest::HttpOpen { request } => {
                    *self.request.lock().unwrap() = Some(request);
                    HttpChunk {
                        handle: "http-1".into(),
                        status: 200,
                        headers: Default::default(),
                        data_base64: String::new(),
                        done: false,
                    }
                }
                ServiceRequest::HttpRead { handle } => {
                    assert_eq!(handle, "http-1");
                    let mut read = self.read.lock().unwrap();
                    let end = (*read + MAX_HTTP_CHUNK).min(self.body.len());
                    let chunk = HttpChunk {
                        handle,
                        status: 200,
                        headers: Default::default(),
                        data_base64: STANDARD.encode(&self.body[*read..end]),
                        done: end == self.body.len(),
                    };
                    *read = end;
                    chunk
                }
                ServiceRequest::HttpClose { handle } => {
                    assert_eq!(handle, "http-1");
                    return Ok(json!({}));
                }
                _ => panic!("unexpected service request"),
            };
            Ok(serde_json::to_value(chunk).unwrap())
        })
    }
}

async fn invoke(frames: &[Value]) -> (ene_plugin_runtime::Result<Value>, Vec<Event>) {
    let (package, _temp) = package();
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "llm".into(), 1, &cancel)
        .await
        .unwrap();
    assert!(instance.capabilities.unwrap().tool_calls);
    let mut body = String::new();
    for frame in frames {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    let services = Arc::new(FixtureHttp {
        body: body.into_bytes(),
        read: Mutex::new(0),
        request: Mutex::new(None),
    });
    let (tx, mut rx) = mpsc::channel(1);
    let run = instance.invoke(
        Operation::Generate {
            request: GenerationRequest {
                endpoint: ENDPOINT.into(),
                options: json!({}),
                model: "fixture".into(),
                preamble: String::new(),
                messages: vec![ChatMessage::text("user", "inspect")],
                tools: vec![ToolSpec {
                    name: "lookup".into(),
                    description: "inspect".into(),
                    parameters: json!({"type": "object"}),
                }],
                max_tokens: 16,
            },
        },
        services.clone(),
        tx,
        &cancel,
    );
    let collect = async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    };
    let outcome = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(run, collect)
    })
    .await
    .expect("native invocation and validated event receiver must finish");
    let request = services.request.lock().unwrap().take().unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, format!("{ENDPOINT}/chat/completions"));
    let body: Value =
        serde_json::from_slice(&STANDARD.decode(request.body_base64.unwrap()).unwrap()).unwrap();
    assert_eq!(body["model"], "fixture");
    assert_eq!(body["stream"], true);
    assert!(*services.read.lock().unwrap() > 0);
    outcome
}

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id": id, "type": "function", "function": {
        "name": name, "arguments": arguments.to_string()
    }})
}

fn indexed_call(index: usize, id: &str, name: &str, arguments: Value) -> Value {
    let mut call = call(id, name, arguments);
    call["index"] = json!(index);
    call
}

fn calls_frame(calls: Vec<Value>) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}}]})
}

fn finish_frame(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn assert_rejected(outcome: (ene_plugin_runtime::Result<Value>, Vec<Event>)) {
    let (result, events) = outcome;
    assert!(result.is_err(), "invalid reply succeeded with {result:?}");
    assert!(events.is_empty(), "invalid reply leaked events {events:?}");
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_preserves_complete_unindexed_call_order() {
    let (result, events) = invoke(&[
        calls_frame(vec![
            call("z", "lookup", json!({"order": 1})),
            call("a", "lookup", json!({"order": 2})),
        ]),
        finish_frame("tool_calls"),
    ])
    .await;
    assert_eq!(result.unwrap(), json!({}));
    assert_eq!(
        serde_json::to_value(events).unwrap(),
        json!([
            {"type": "tool_call", "id": "z", "name": "lookup", "arguments": {"order": 1}},
            {"type": "tool_call", "id": "a", "name": "lookup", "arguments": {"order": 2}},
            {"type": "finished", "reason": "tool_calls"}
        ])
    );
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_assembles_indexed_fragments_before_emitting() {
    let (result, events) = invoke(&[
        calls_frame(vec![
            json!({"index": 1, "id": "second", "function": {"name": "look", "arguments": "{\"b\":"}}),
            json!({"index": 0, "id": "first", "function": {"name": "lookup", "arguments": "{\"a\":"}}),
        ]),
        calls_frame(vec![
            json!({"index": 1, "function": {"name": "up", "arguments": "2}"}}),
            json!({"index": 0, "function": {"arguments": "1}"}}),
        ]),
        finish_frame("tool_calls"),
    ])
    .await;
    assert_eq!(result.unwrap(), json!({}));
    assert_eq!(
        serde_json::to_value(events).unwrap(),
        json!([
            {"type": "tool_call", "id": "first", "name": "lookup", "arguments": {"a": 1}},
            {"type": "tool_call", "id": "second", "name": "lookup", "arguments": {"b": 2}},
            {"type": "finished", "reason": "tool_calls"}
        ])
    );
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_accepts_64_calls_256_byte_metadata_and_128_byte_reason() {
    let name = "n".repeat(256);
    let ids: Vec<_> = (0..64)
        .map(|index| format!("{index:03}{}", "i".repeat(253)))
        .collect();
    let reason = "r".repeat(128);
    let (result, events) = invoke(&[
        calls_frame(
            ids.iter()
                .enumerate()
                .map(|(index, id)| indexed_call(index, id, &name, json!({"index": index})))
                .collect(),
        ),
        finish_frame(&reason),
    ])
    .await;
    assert_eq!(result.unwrap(), json!({}));
    assert_eq!(events.len(), 65);
    for (index, event) in events[..64].iter().enumerate() {
        assert!(
            matches!(event, Event::ToolCall { id, name: actual_name, arguments }
            if id == &ids[index] && actual_name == &name && arguments == &json!({"index": index}))
        );
    }
    assert!(matches!(&events[64], Event::Finished { reason: actual } if actual == &reason));
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_rejects_257_byte_metadata_without_leaking_first_call() {
    for (id, name) in [
        ("i".repeat(257), "lookup".into()),
        ("second".into(), "n".repeat(257)),
    ] {
        assert_rejected(
            invoke(&[
                calls_frame(vec![
                    indexed_call(0, "first", "lookup", json!({})),
                    indexed_call(1, &id, &name, json!({})),
                ]),
                finish_frame("tool_calls"),
            ])
            .await,
        );
    }
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_rejects_65_calls_without_leaking_first_call() {
    assert_rejected(
        invoke(&[
            calls_frame(vec![indexed_call(0, "first", "lookup", json!({}))]),
            calls_frame(
                (1..65)
                    .map(|index| indexed_call(index, &format!("call-{index}"), "lookup", json!({})))
                    .collect(),
            ),
            finish_frame("tool_calls"),
        ])
        .await,
    );
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_rejects_129_byte_finish_reason_without_leaking_calls() {
    assert_rejected(
        invoke(&[
            calls_frame(vec![indexed_call(0, "first", "lookup", json!({}))]),
            finish_frame(&"r".repeat(129)),
        ])
        .await,
    );
}

fn sized_arguments(event_bytes: usize) -> Value {
    let event =
        json!({"type": "tool_call", "id": "second", "name": "lookup", "arguments": {"x": ""}});
    let overhead = serde_json::to_vec(&event).unwrap().len();
    let arguments = json!({"x": "x".repeat(event_bytes - overhead)});
    assert_eq!(
        serde_json::to_vec(&Event::ToolCall {
            id: "second".into(),
            name: "lookup".into(),
            arguments: arguments.clone(),
        })
        .unwrap()
        .len(),
        event_bytes
    );
    arguments
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_accepts_an_exact_128_kib_tool_event() {
    let arguments = sized_arguments(128 * 1024);
    let (result, events) = invoke(&[
        calls_frame(vec![indexed_call(0, "second", "lookup", arguments.clone())]),
        finish_frame("tool_calls"),
    ])
    .await;
    assert_eq!(result.unwrap(), json!({}));
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], Event::ToolCall { id, name, arguments: actual }
        if id == "second" && name == "lookup" && actual == &arguments)
    );
    assert!(matches!(&events[1], Event::Finished { reason } if reason == "tool_calls"));
}

#[tokio::test]
#[ignore = "requires ENE_OPENROUTER_WASM pointing to a freshly built absolute Wasm path"]
async fn native_openrouter_rejects_an_oversized_tool_event_without_leaking_first_call() {
    assert_rejected(
        invoke(&[
            calls_frame(vec![
                indexed_call(0, "first", "lookup", json!({})),
                indexed_call(1, "second", "lookup", sized_arguments(128 * 1024 + 1)),
            ]),
            finish_frame("tool_calls"),
        ])
        .await,
    );
}
