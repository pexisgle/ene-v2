//! The real provider path: rig's OpenAI wire through the in-process client,
//! the Extism guest, the host broker, and a local OpenAI-compatible fixture.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::plugin_completion;
use crate::host::{
    model::Credentials,
    providers::{Grant, Providers, Running, http_broker},
    settings::{
        Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
    },
};
use ene_plugin::Extension;
use ene_plugin_runtime::service::HttpGrants;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use rig_core::{
    completion::{CompletionRequest, FinishReason, ToolDefinition, message::ToolName},
    driver::DynModel,
    operation::Completion,
    streaming::{Item, StreamEvent},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};
use tokio_util::sync::CancellationToken;
use url::Url;

const SECRET: &str = "test-secret";
const CONNECTION: &str = "fixture-connection";

fn wasm() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm");
    assert!(
        path.is_file(),
        "build the guest first: cargo build --release --target wasm32-unknown-unknown -p ene-openrouter-wasm ({})",
        path.display()
    );
    path
}

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// A local OpenAI-compatible provider. It rejects nothing itself; the broker
/// decides whether the request is allowed and what credential it carries.
struct Fixture {
    base: String,
    auth: Arc<Mutex<Vec<Option<String>>>>,
    requests: Arc<AtomicUsize>,
    closed: Arc<Notify>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let auth = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let task = {
            let base = base.clone();
            let auth = Arc::clone(&auth);
            let requests = Arc::clone(&requests);
            let closed = Arc::clone(&closed);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let auth = Arc::clone(&auth);
                    let requests = Arc::clone(&requests);
                    let closed = Arc::clone(&closed);
                    let cancel = cancel.clone();
                    let base = base.clone();
                    tokio::spawn(async move {
                        let request = read_request(&mut stream).await;
                        let Some(request) = request else {
                            return;
                        };
                        requests.fetch_add(1, Ordering::SeqCst);
                        auth.lock()
                            .unwrap()
                            .push(request.headers.get("authorization").cloned());
                        serve(&mut stream, &request, &base, &closed, &cancel).await;
                    });
                }
            })
        };
        Self {
            base,
            auth,
            requests,
            closed,
            cancel,
            task,
        }
    }

    fn base(&self) -> &str {
        &self.base
    }

    fn seen_auth(&self) -> Vec<Option<String>> {
        self.auth.lock().unwrap().clone()
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn closed(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.closed.notified())
            .await
            .expect("the cancelled exchange closes its connection");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

struct Request {
    method: String,
    path: String,
    headers: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let first = lines.next()?.split_whitespace().collect::<Vec<_>>();
    let method = first.first()?.to_string();
    let path = first.get(1)?.to_string();
    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body).await.ok()?;
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

async fn serve(
    stream: &mut tokio::net::TcpStream,
    request: &Request,
    base: &str,
    closed: &Notify,
    cancel: &CancellationToken,
) {
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let model = body["model"].as_str().unwrap_or("test-normal");
    let streaming = body["stream"].as_bool().unwrap_or(false);
    let _ = (request.method.as_str(), request.path.as_str(), base);
    match model {
        "test-error" => {
            write_json(
                stream,
                500,
                &json!({"error": {"message": "fixture failed"}}),
            )
            .await;
        }
        "test-large" => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16777217\r\n\r\n";
            let _ = stream.write_all(head.as_bytes()).await;
        }
        "test-broken" => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                      data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-broken\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                )
                .await
                .unwrap();
            let _ = stream.flush().await;
            let _ = stream.write_all(b"data: {\"broken\":\n\n").await;
        }
        "test-slow" => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                      data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-slow\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
                )
                .await
                .unwrap();
            let _ = stream.flush().await;
            let mut drain = [0u8; 1];
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = stream.read(&mut drain) => {}
            }
            closed.notify_one();
        }
        "test-delayed" => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                      data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-delayed\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
                )
                .await
                .unwrap();
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let chunk = json!({"id":"1","object":"chat.completion.chunk","created":1,"model":"test-delayed","choices":[{"index":0,"delta":{"content":" second"},"finish_reason":"stop"}]});
            let _ = stream
                .write_all(format!("data: {chunk}\n\n").as_bytes())
                .await;
            let _ = stream.write_all(b"data: [DONE]\n\n").await;
        }
        "test-tools" => {
            if streaming {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                    .await
                    .unwrap();
                let chunks = [
                    json!({"id":"1","object":"chat.completion.chunk","created":1,"model":"test-tools","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":""}}]},"finish_reason":null}]}),
                    json!({"id":"1","object":"chat.completion.chunk","created":1,"model":"test-tools","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]},"finish_reason":null}]}),
                    json!({"id":"1","object":"chat.completion.chunk","created":1,"model":"test-tools","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":null}]}),
                    json!({"id":"1","object":"chat.completion.chunk","created":1,"model":"test-tools","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}),
                ];
                for chunk in chunks {
                    let _ = stream
                        .write_all(format!("data: {chunk}\n\n").as_bytes())
                        .await;
                }
                let _ = stream.write_all(b"data: [DONE]\n\n").await;
            } else {
                write_json(
                    stream,
                    200,
                    &json!({
                        "id":"1","object":"chat.completion","created":1,"model":"test-tools",
                        "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"x\":1}"}}]},"finish_reason":"tool_calls"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
                    }),
                )
                .await;
            }
        }
        _ => {
            if streaming {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                    .await
                    .unwrap();
                for text in ["Hello", " world"] {
                    let chunk = json!({"id":"1","object":"chat.completion.chunk","created":1,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
                    let _ = stream
                        .write_all(format!("data: {chunk}\n\n").as_bytes())
                        .await;
                }
                let stop = json!({"id":"1","object":"chat.completion.chunk","created":1,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                let _ = stream
                    .write_all(format!("data: {stop}\n\n").as_bytes())
                    .await;
                let usage = json!({"id":"1","object":"chat.completion.chunk","created":1,"model":model,"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}});
                let _ = stream
                    .write_all(format!("data: {usage}\n\n").as_bytes())
                    .await;
                let _ = stream.write_all(b"data: [DONE]\n\n").await;
            } else {
                write_json(
                    stream,
                    200,
                    &json!({
                        "id":"1","object":"chat.completion","created":1,"model":model,
                        "choices":[{"index":0,"message":{"role":"assistant","content":"Hello world"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
                    }),
                )
                .await;
            }
        }
    }
    let _ = stream.flush().await;
}

async fn write_json(stream: &mut tokio::net::TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Records the headers the guest asked the broker to send, before the real
/// Grant attaches the credential.
struct Recording {
    inner: Grant,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
}

impl HttpGrants for Recording {
    fn allows(&self, provider: &str, url: &Url) -> bool {
        self.inner.allows(provider, url)
    }

    fn authorize(
        &self,
        provider: &str,
        headers: &mut HeaderMap,
    ) -> Result<(), ene_plugin::Failure> {
        self.seen.lock().unwrap().push(headers.clone());
        self.inner.authorize(provider, headers)
    }
}

struct Harness {
    fixture: Fixture,
    _temp: TempDir,
    running: Arc<Running>,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
}

impl Harness {
    async fn start(destinations: Option<Vec<String>>) -> Self {
        let fixture = Fixture::start().await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugins").join("openrouter-test");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(
                temp.path().join("plugins"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        write_private(&root.join("provider.wasm"), std::fs::read(wasm()).unwrap());
        let destinations = destinations.unwrap_or_else(|| vec![fixture.base().to_owned()]);
        write_private(
            &root.join("manifest.json"),
            json!({
                "contract_version": ene_plugin::CONTRACT_VERSION,
                "id": "openrouter-test",
                "name": "OpenRouter fixture",
                "version": "1",
                "entrypoints": [{
                    "id": "llm", "target": "host", "file": "provider.wasm",
                    "provides": ["llm"], "uses": ["http"],
                    "provider": {
                        "id": "openrouter-test",
                        "endpoint": fixture.base(),
                        "auth": {"type": "bearer"},
                        "destinations": destinations,
                    }
                }]
            })
            .to_string(),
        );
        let providers = Providers::load(temp.path());
        let running = providers
            .start("openrouter-test", Extension::Llm)
            .await
            .unwrap();
        Self {
            fixture,
            _temp: temp,
            running,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn model(&self, model: &str) -> DynModel<Completion> {
        let credentials = Credentials::new(true);
        credentials.remember(CONNECTION, SECRET.to_owned());
        let connection = Connection {
            id: CONNECTION.to_owned(),
            name: "fixture".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openrouter-test".into(),
                    endpoint: self.fixture.base().to_owned(),
                    ..Default::default()
                },
                model_ids: Vec::new(),
            }),
            credential_state: CredentialState::Registered,
            revision: 1,
            last_check: None,
        };
        let grant = Grant::new(self.running.provider(), &connection, Some(&credentials));
        let recording = Arc::new(Recording {
            inner: grant,
            seen: Arc::clone(&self.seen),
        });
        let broker = http_broker(recording as Arc<dyn HttpGrants>).unwrap();
        plugin_completion(Arc::clone(&self.running), model.into(), broker)
    }

    fn seen(&self) -> Vec<HeaderMap> {
        self.seen.lock().unwrap().clone()
    }
}

fn request() -> CompletionRequest {
    CompletionRequest::new("hello")
}

fn tool_request() -> CompletionRequest {
    let mut request = CompletionRequest::new("hello");
    request.tools.push(Arc::new(ToolDefinition {
        name: ToolName::new("lookup").unwrap(),
        description: "inspect".into(),
        parameters: json!({"type": "object"}),
        annotations: Default::default(),
    }));
    request
}

#[tokio::test]
async fn normal_response_usage_and_credential_injection() {
    let harness = Harness::start(None).await;
    let model: DynModel<Completion> = harness.model("test-normal");
    let reply = model.call(request()).await.unwrap();
    assert_eq!(reply.text(), "Hello world");
    assert_eq!(reply.usage.total_tokens, Some(3));
    assert_eq!(reply.finish_reason(), Some(FinishReason::Stop));
    assert_eq!(
        harness.fixture.seen_auth(),
        vec![Some(format!("Bearer {SECRET}"))]
    );
    // The guest asked the broker for these headers; the credential is not
    // among them, and the broker attached it afterwards.
    for headers in harness.seen() {
        assert!(!headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn streaming_response() {
    let harness = Harness::start(None).await;
    let model: DynModel<Completion> = harness.model("test-normal");
    let mut stream = model.stream(request()).unwrap();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let Item::Event(StreamEvent::Text { text: delta, .. }) = item.unwrap() {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "Hello world");
}

#[tokio::test]
async fn split_tool_arguments_are_assembled_by_rig() {
    let harness = Harness::start(None).await;
    let reply = harness
        .model("test-tools")
        .call(tool_request())
        .await
        .unwrap();
    let call = reply
        .choice
        .iter()
        .find_map(|content| match content {
            rig_core::completion::message::AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("a tool call");
    assert_eq!(call.function.name.to_string(), "lookup");
    assert_eq!(call.function.arguments["x"], 1);
    assert_eq!(reply.finish_reason(), Some(FinishReason::ToolCalls));
}

#[tokio::test]
async fn mid_stream_failure_is_an_error() {
    let harness = Harness::start(None).await;
    let mut stream = harness.model("test-broken").stream(request()).unwrap();
    let mut failed = false;
    while let Some(item) = stream.next().await {
        if item.is_err() {
            failed = true;
        }
    }
    assert!(failed, "a broken stream must surface an error");
}

#[tokio::test]
async fn streaming_arrives_before_the_provider_finishes() {
    let harness = Harness::start(None).await;
    let start = std::time::Instant::now();
    let mut stream = harness.model("test-delayed").stream(request()).unwrap();
    let first = stream.next().await;
    let elapsed = start.elapsed();
    assert!(first.is_some());
    assert!(
        elapsed < Duration::from_millis(350),
        "the first delta waited for the whole reply: {elapsed:?}"
    );
}

#[tokio::test]
async fn cancellation_closes_the_exchange_and_keeps_the_instance_usable() {
    let harness = Harness::start(None).await;
    let mut stream = harness.model("test-slow").stream(request()).unwrap();
    let first = stream.next().await;
    assert!(
        first.is_some(),
        "the first chunk arrives before cancellation"
    );
    drop(stream);
    harness.fixture.closed().await;
    // The same provider instance answers another call, so the cancelled
    // invocation released the guest, the broker and the instance.
    let reply = harness.model("test-normal").call(request()).await.unwrap();
    assert_eq!(reply.text(), "Hello world");
}

#[tokio::test]
async fn a_destination_outside_the_grant_is_refused() {
    let harness = Harness::start(Some(vec!["http://127.0.0.1:1".into()])).await;
    let result = harness.model("test-normal").call(request()).await;
    assert!(result.is_err(), "the broker must refuse the destination");
    assert_eq!(harness.fixture.request_count(), 0);
}

#[tokio::test]
async fn a_reply_over_the_body_cap_is_refused() {
    let harness = Harness::start(None).await;
    let result = harness.model("test-large").call(request()).await;
    assert!(result.is_err(), "the broker must refuse an oversized reply");
}

#[tokio::test]
async fn a_non_success_status_is_an_error() {
    let harness = Harness::start(None).await;
    let result = harness.model("test-error").call(request()).await;
    assert!(result.is_err(), "a non-2xx reply is a transport failure");
}
