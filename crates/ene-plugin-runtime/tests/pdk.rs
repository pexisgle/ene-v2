//! The compiled Extism PDK reference executes through the embedded Extism Host SDK.
use base64::Engine;
use ene_plugin::*;
use ene_plugin_runtime::{
    Package,
    instance::{Instance, Services},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
struct HttpFixture(Mutex<VecDeque<Vec<u8>>>);
impl Services for HttpFixture {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            if let ServiceRequest::HttpClose { handle } = &request {
                assert_eq!(handle, "stream");
                let mut chunks = self.0.lock().await;
                assert_eq!(*chunks, VecDeque::from([vec![]]));
                chunks.clear();
                return Ok(json!({}));
            }
            assert!(matches!(
                request,
                ServiceRequest::HttpOpen { .. } | ServiceRequest::HttpRead { .. }
            ));
            let mut chunks = self.0.lock().await;
            let chunk = chunks
                .pop_front()
                .expect("guest read past stream completion");
            Ok(
                json!({"handle":"stream","status":200,"headers":{},"data_base64":base64::engine::general_purpose::STANDARD.encode(chunk),"done":chunks.is_empty()}),
            )
        })
    }
}
#[tokio::test]
#[ignore = "build ene-openrouter-wasm for wasm32-unknown-unknown and set ENE_TEST_PDK_WASM"]
async fn reference_pdk_models_split_utf8_streaming_and_shutdown() {
    let wasm =
        std::fs::read(std::env::var_os("ENE_TEST_PDK_WASM").expect("ENE_TEST_PDK_WASM")).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("package");
    ene_platform::storage::create_private_dir_all(&root).unwrap();
    ene_platform::storage::write_atomic(
        &root.join("manifest.json"),
        include_bytes!("../../../examples/plugins/openrouter-wasm/manifest.json"),
    )
    .unwrap();
    ene_platform::storage::write_atomic(&root.join("provider.wasm"), &wasm).unwrap();
    let package = Package::load(&root).unwrap();
    let cancel = CancellationToken::new();
    let mut worker = Instance::load(package, "llm".into(), Target::Host, 7, &cancel)
        .await
        .unwrap();
    for models in [true, false] {
        let endpoint = "https://openrouter.ai/api/v1".to_string();
        let (operation, payload) = if models {
            (
                Operation::ListModels {
                    endpoint,
                    options: json!({}),
                },
                json!({"data":[{"id":"fixture","name":"Fixture","context_length":8192,"top_provider":{"max_completion_tokens":256}}]})
                    .to_string()
                    .into_bytes(),
            )
        } else {
            (
                Operation::Generate {
                    request: GenerationRequest {
                        endpoint,
                        options: json!({}),
                        model: "fixture".into(),
                        preamble: "".into(),
                        messages: vec![ene_plugin::provider::ChatMessage {
                            role: "user".into(),
                            text: "hello".into(),
                        }],
                        max_tokens: 64,
                        tools: vec![],
                    },
                },
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n",
                    json!({"choices":[{"delta":{"content":"こんにちは"},"finish_reason":null}]}),
                    json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                )
                .into_bytes(),
            )
        };
        let mut chunks: VecDeque<_> = payload.chunks(13).map(Vec::from).collect();
        chunks.push_back(vec![]);
        let services = Arc::new(HttpFixture(Mutex::new(chunks)));
        let (events, mut received) = tokio::sync::mpsc::channel(64);
        worker
            .invoke(operation, services.clone(), events, &cancel)
            .await
            .unwrap();
        assert!(services.0.lock().await.is_empty());
        let mut output = Vec::new();
        while let Some(event) = received.recv().await {
            output.push(event)
        }
        if models {
            let Event::Models { models } = &output[0] else {
                panic!("model list missing");
            };
            assert_eq!(models[0].id, "fixture");
            let metadata = models[0]
                .metadata
                .as_option()
                .expect("model metadata missing");
            assert_eq!(metadata.context_window, Some(8192));
            assert_eq!(metadata.max_output_tokens, Some(256));
        } else {
            let text: String = output
                .iter()
                .filter_map(|e| {
                    if let Event::Delta { text } = e {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(text, "こんにちは");
            assert!(matches!(output.last(),Some(Event::Finished {reason}) if reason=="stop"));
        }
    }
    worker.stop().await;
}
