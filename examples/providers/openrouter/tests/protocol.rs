use base64::{Engine, engine::general_purpose::STANDARD};
use ene_protocol::ipc::{Reader, Writer};
use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Plugin {
    child: Child,
    input: Writer<ChildStdin>,
    output: Reader<ChildStdout>,
}
impl Plugin {
    async fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ene-provider-openrouter"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut plugin = Self {
            input: Writer::with_limit(
                child.stdin.take().unwrap(),
                ene_protocol::provider::MAX_LINE,
            ),
            output: Reader::with_limit(
                child.stdout.take().unwrap(),
                ene_protocol::provider::MAX_LINE,
            ),
            child,
        };
        plugin
            .send(json!({"type":"initialize", "request_id":"init", "protocol_version":1}))
            .await;
        let response = plugin.receive().await;
        assert_eq!(response["type"], "initialized");
        assert_eq!(
            response["capabilities"],
            json!({"streaming_text":true, "model_list":true})
        );
        plugin
    }
    async fn send(&mut self, message: Value) {
        self.input.send(&message).await.unwrap();
    }
    async fn receive(&mut self) -> Value {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.output.receive::<Value>(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    }
    async fn response(&mut self, id: &str, bytes: &[u8], done: bool) {
        self.send(
            json!({"type":"http_response", "request_id":id, "http_id":"http-1",
            "status":200, "headers":{}, "data_base64":STANDARD.encode(bytes), "done":done}),
        )
        .await;
    }
    async fn stop(mut self) {
        self.send(json!({"type":"shutdown"})).await;
        drop(self.input);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

fn generation(id: &str) -> Value {
    json!({"type":"generate", "request_id":id, "request":{
        "endpoint":"https://openrouter.ai/api/v1/", "options":{"upstream":"openai", "allow_fallbacks":false},
        "model":"model", "preamble":"system", "messages":[{"role":"user", "text":"hello"}], "max_tokens":128}})
}

#[tokio::test]
async fn generation_preserves_routing_and_unicode_across_http_chunks() {
    let mut plugin = Plugin::start().await;
    plugin.send(generation("g")).await;
    let http = plugin.receive().await;
    assert_eq!(http["type"], "http_request");
    assert_eq!(http["method"], "POST");
    assert_eq!(http["url"], "https://openrouter.ai/api/v1/chat/completions");
    let body: Value = serde_json::from_slice(
        &STANDARD
            .decode(http["body_base64"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["provider"],
        json!({"only":["openai"], "allow_fallbacks":false})
    );
    assert_eq!(
        body["messages"],
        json!([{"role":"system","content":"system"},{"role":"user","content":"hello"}])
    );
    let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"日本🙂\"},\"finish_reason\":null}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]";
    for byte in stream.as_bytes() {
        plugin.response("g", &[*byte], false).await;
    }
    plugin.response("g", &[], true).await;
    assert_eq!(
        plugin.receive().await,
        json!({"type":"delta", "request_id":"g", "text":"日本🙂"})
    );
    assert_eq!(
        plugin.receive().await,
        json!({"type":"finished", "request_id":"g", "reason":"stop"})
    );
    plugin.stop().await;
}

#[tokio::test]
async fn model_listing_cancellation_and_failure_release_request_state() {
    let mut plugin = Plugin::start().await;
    plugin.send(generation("cancelled")).await;
    assert_eq!(plugin.receive().await["type"], "http_request");
    plugin
        .send(json!({"type":"cancel", "request_id":"cancelled"}))
        .await;
    // A late broker response must not produce a result after cancellation.
    plugin.response("cancelled", b"data: broken\n", true).await;
    plugin.send(json!({"type":"list_models", "request_id":"models", "endpoint":"https://openrouter.ai/api/v1", "options":{}})).await;
    let http = plugin.receive().await;
    assert_eq!(http["url"], "https://openrouter.ai/api/v1/models");
    assert_eq!(http["method"], "GET");
    let models = "{\"data\":[{\"id\":\"a\",\"name\":\"日本\"},{\"id\":\"b\"}]}";
    for chunk in models.as_bytes().chunks(2) {
        plugin.response("models", chunk, false).await;
    }
    plugin.response("models", &[], true).await;
    assert_eq!(
        plugin.receive().await,
        json!({"type":"models", "request_id":"models", "models":[{"id":"a","name":"日本"},{"id":"b","name":"b"}]})
    );
    plugin.send(generation("bad")).await;
    assert_eq!(plugin.receive().await["type"], "http_request");
    plugin
        .response(
            "bad",
            b"data: {\"error\":{\"message\":\"secret upstream detail\"}}\n",
            true,
        )
        .await;
    let error = plugin.receive().await;
    assert_eq!(
        error,
        json!({"type":"error", "request_id":"bad", "code":"provider_failure", "message":"Provider request failed"})
    );
    plugin.send(generation("next")).await;
    assert_eq!(plugin.receive().await["request_id"], "next");
    plugin.stop().await;
}
