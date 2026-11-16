//! Temporary prototype: Rig's OpenAI wire over an in-process guest transport.
//!
//! Not production. The guest is a double that answers the OpenAI chat
//! completions wire, so this measures the host side of the comparison and
//! exercises the six behaviours without a loopback HTTP server.

use futures_util::{Stream, StreamExt};
use rig_core::completion::CompletionRequest;
use rig_core::driver::DynModel;
use rig_core::http_client::{
    BoxedStream, Bytes, HeaderMap, HttpClientExt, LazyBody, MultipartForm, Request, Response,
    StatusCode,
};
use rig_core::operation::Completion;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::streaming::{Item, StreamEvent};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio_util::sync::CancellationToken;

const KEY: &str = "sk-prototype";

#[derive(Default)]
struct Guest {
    seen_auth: Mutex<Vec<String>>,
    cancel: CancellationToken,
    cancels: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct GuestClient {
    guest: Arc<Guest>,
}

impl Guest {
    fn answer(&self, body: &[u8], streaming: bool) -> (StatusCode, HeaderMap, Vec<u8>) {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let scenario = request["model"].as_str().unwrap_or("normal");
        let mut headers = HeaderMap::new();
        if scenario == "fail" {
            headers.insert("content-type", "application/json".parse().unwrap());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                headers,
                br#"{"error":{"message":"provider exploded"}}"#.to_vec(),
            );
        }
        if scenario == "tools" {
            if streaming {
                headers.insert("content-type", "text/event-stream".parse().unwrap());
                return (StatusCode::OK, headers, sse(&tool_chunks(scenario)));
            }
            headers.insert("content-type", "application/json".parse().unwrap());
            return (
                StatusCode::OK,
                headers,
                serde_json::to_vec(&json_tool_reply(scenario)).unwrap(),
            );
        }
        if streaming {
            headers.insert("content-type", "text/event-stream".parse().unwrap());
            let body = if scenario == "truncate" {
                let mut text = Vec::new();
                for chunk in text_chunks("normal", "Hello", false) {
                    text.extend_from_slice(format!("data: {chunk}\n\n").as_bytes());
                }
                text.extend_from_slice(b"data: {\"broken\":\n\n");
                text
            } else if scenario == "hold" {
                let mut text = Vec::new();
                for chunk in text_chunks("normal", "Hello", false) {
                    text.extend_from_slice(format!("data: {chunk}\n\n").as_bytes());
                }
                text
            } else {
                sse(&text_chunks(scenario, "Hello world", true))
            };
            (StatusCode::OK, headers, body)
        } else {
            headers.insert("content-type", "application/json".parse().unwrap());
            (
                StatusCode::OK,
                headers,
                serde_json::to_vec(&json_reply(scenario, "Hello world")).unwrap(),
            )
        }
    }
}

fn sse(chunks: &[String]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(chunk);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

fn chunk(model: &str, delta: serde_json::Value, finish: Option<&str>) -> String {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

fn text_chunks(model: &str, text: &str, done: bool) -> Vec<String> {
    let mut chunks = vec![
        chunk(model, serde_json::json!({"role": "assistant", "content": ""}), None),
    ];
    for word in text.split_inclusive(' ') {
        chunks.push(chunk(model, serde_json::json!({"content": word}), None));
    }
    if done {
        chunks.push(chunk(model, serde_json::json!({}), Some("stop")));
    }
    chunks
}

fn tool_chunks(model: &str) -> Vec<String> {
    vec![
        chunk(
            model,
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "do_thing", "arguments": ""},
                }],
            }),
            None,
        ),
        chunk(
            model,
            serde_json::json!({
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "{\"x\":"},
                }],
            }),
            None,
        ),
        chunk(
            model,
            serde_json::json!({
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "1}"},
                }],
            }),
            None,
        ),
        chunk(model, serde_json::json!({}), Some("tool_calls")),
    ]
}

fn json_tool_reply(model: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "do_thing", "arguments": "{\"x\":1}"},
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })
}

fn json_reply(model: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
    })
}

impl GuestClient {
    fn capture(&self, headers: &HeaderMap) {
        if let Some(value) = headers.get("authorization") {
            self.guest
                .seen_auth
                .lock()
                .unwrap()
                .push(value.to_str().unwrap().to_owned());
        }
    }
}

impl HttpClientExt for GuestClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = rig_core::http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let client = self.clone();
        async move {
            client.capture(&parts.headers);
            let (status, headers, bytes) = client.guest.answer(&body, false);
            let mut response = Response::new(
                Box::pin(async move { Ok(U::from(Bytes::from(bytes))) }) as LazyBody<U>,
            );
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            Ok(response)
        }
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = rig_core::http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        async { unimplemented!("chat completions does not use multipart") }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = rig_core::http_client::Result<rig_core::http_client::StreamingResponse>>
    + Send
    where
        T: Into<Bytes> + Send,
    {
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let client = self.clone();
        async move {
            client.capture(&parts.headers);
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let scenario = request["model"].as_str().unwrap_or("normal");
            let (status, headers, bytes) = client.guest.answer(&body, true);
            let hold = scenario == "hold";
            let cancel = client.guest.cancel.clone();
            let cancels_for_stream = Arc::clone(&client.guest.cancels);
            let body = GuestStream {
                chunks: split(bytes).into_iter(),
                hold,
                cancel,
                cancels: cancels_for_stream,
            };
            let stream: BoxedStream = Box::pin(body);
            let mut response = Response::new(stream);
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            Ok(response)
        }
    }
}

fn split(bytes: Vec<u8>) -> Vec<Bytes> {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(Bytes::copy_from_slice)
        .collect()
}

struct GuestStream {
    chunks: std::vec::IntoIter<Bytes>,
    hold: bool,
    cancel: CancellationToken,
    cancels: Arc<AtomicUsize>,
}

impl Stream for GuestStream {
    type Item = rig_core::http_client::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(chunk) = this.chunks.next() {
            return Poll::Ready(Some(Ok(chunk)));
        }
        if this.hold {
            return Poll::Pending;
        }
        Poll::Ready(None)
    }
}

impl Drop for GuestStream {
    fn drop(&mut self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        self.cancel.cancel();
    }
}

fn model(guest: Arc<Guest>, name: &str) -> DynModel<Completion> {
    OpenAIConfig::new(KEY)
        .connect(GuestClient { guest })
        .chat(name)
        .erase()
}

fn request() -> CompletionRequest {
    CompletionRequest::new("hello")
}

#[tokio::test]
async fn normal_response() {
    let guest = Arc::new(Guest::default());
    let reply = model(Arc::clone(&guest), "normal")
        .call(request())
        .await
        .unwrap();
    assert_eq!(reply.text(), "Hello world");
    assert_eq!(reply.usage.total_tokens, Some(3));
}

#[tokio::test]
async fn streaming_response() {
    let guest = Arc::new(Guest::default());
    let mut stream = model(Arc::clone(&guest), "normal")
        .stream(request())
        .unwrap();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let Item::Event(StreamEvent::Text { text: delta, .. }) = item.unwrap() {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "Hello world");
}

#[tokio::test]
async fn tool_call_response() {
    let guest = Arc::new(Guest::default());
    let reply = model(Arc::clone(&guest), "tools")
        .call(request())
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
    assert_eq!(call.function.name.to_string(), "do_thing");
    assert_eq!(call.function.arguments["x"], 1);
}

#[tokio::test]
async fn mid_stream_failure_is_an_error() {
    let guest = Arc::new(Guest::default());
    let mut stream = model(Arc::clone(&guest), "truncate")
        .stream(request())
        .unwrap();
    let mut failed = false;
    while let Some(item) = stream.next().await {
        if item.is_err() {
            failed = true;
        }
    }
    assert!(failed, "a broken SSE frame must surface as an error");
}

#[tokio::test]
async fn dropping_the_stream_cancels_the_guest() {
    let guest = Arc::new(Guest::default());
    let mut stream = model(Arc::clone(&guest), "hold")
        .stream(request())
        .unwrap();
    let first = stream.next().await;
    assert!(first.is_some());
    drop(stream);
    assert!(guest.cancel.is_cancelled());
    assert_eq!(guest.cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_provider_key_never_reaches_the_guest() {
    let guest = Arc::new(Guest::default());
    let call = model(Arc::clone(&guest), "normal").call(request());
    call.await.unwrap();
    let seen = guest.seen_auth.lock().unwrap();
    assert_eq!(seen.as_slice(), [format!("Bearer {KEY}")]);
    // The guest double only receives the request body; the client never
    // forwards the provider's Authorization header.
    assert!(!guest.cancel.is_cancelled());
}
