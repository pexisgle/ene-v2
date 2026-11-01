use super::*;
use crate::infrastructure::llm::plugin::manifest;
use crate::{domain::OperationKind, llm::test_support::InvocationFixture};
use ene_plugin::{GenerationRequest, Operation, provider::ChatMessage};
use tokio::sync::mpsc;

#[tokio::test]
async fn pdk_uses_authorized_catalog_and_streams_with_large_history() {
    use axum::{
        Json, Router,
        body::Body,
        http::HeaderMap,
        routing::{get, post},
    };
    let router = Router::new()
        .route("/api/v1/models", get(|uri: axum::http::Uri| async move {
            assert_eq!(uri.query(), Some("providers=openai"));
            Json(json!({"data":[{"id":"fixture","context_length":8192}]}))
        }))
        .route("/api/v1/chat/completions", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer fixture-secret");
            assert_eq!(body["provider"], json!({"only":["openai"],"allow_fallbacks":false}));
            assert_eq!(body["messages"][0]["content"].as_str().unwrap().len(), 200 * 1024);
            let response = "data: {\"choices\":[{\"delta\":{\"content\":\"こんにちは\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let chunks: Vec<_> = response.as_bytes().chunks(13)
                .map(|chunk| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(chunk))).collect();
            Body::from_stream(futures_util::stream::iter(chunks))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("pdk");
    ene_platform::storage::create_private_dir_all(&root).unwrap();
    ene_platform::storage::write_atomic(
        &root.join("manifest.json"),
        include_bytes!("../../../../../../examples/plugins/openrouter-wasm/manifest.json"),
    )
    .unwrap();
    let wasm = std::env::var_os("ENE_TEST_PDK_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm")
        });
    ene_platform::storage::write_atomic(
        &root.join("provider.wasm"),
        &std::fs::read(wasm).expect("build ene-openrouter-wasm before host tests"),
    )
    .unwrap();
    let package = Package::load(&root).unwrap();
    let mut bundle = super::super::bundle(&package, "llm").unwrap();
    manifest::validate(&bundle.manifest).unwrap();
    // Keep the shipped routing/auth rules, using a local upstream for this test.
    bundle.manifest.destinations = vec![origin.clone()];
    let cancel = CancellationToken::new();
    let mut worker = Instance::load(package, "llm".into(), Target::Host, 1, &cancel)
        .await
        .unwrap();
    for models in [true, false] {
        let endpoint = format!("{origin}/api/v1");
        let (_stop, receiver) = tokio::sync::watch::channel(false);
        let input = Arc::new(
            InvocationFixture {
                id: "pdk".into(),
                operation: if models {
                    OperationKind::ModelList
                } else {
                    OperationKind::Conversation
                },
                connection: Connection {
                    endpoint: endpoint.clone(),
                    ..Default::default()
                },
                request: None,
                secret: Some(crate::ports::Secret {
                    value: "fixture-secret".into(),
                    nonce: "n".into(),
                }),
                authorization: Arc::new(super::super::tests::Permit),
                cancel: receiver,
            }
            .with_destinations(vec![origin.clone()]),
        );
        let services = Arc::new(ProviderServices {
            input: input.clone(),
            streams: Mutex::new(BTreeMap::new()),
            broker: Arc::new(broker::Broker {
                bundle: bundle.clone(),
                input,
                calls: Default::default(),
                active: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
        });
        let operation = if models {
            Operation::ListModels {
                endpoint,
                options: json!({}),
            }
        } else {
            Operation::Generate {
                request: GenerationRequest {
                    endpoint,
                    options: json!({}),
                    model: "fixture".into(),
                    preamble: String::new(),
                    messages: vec![ChatMessage {
                        role: "user".into(),
                        text: "x".repeat(200 * 1024),
                    }],
                    max_tokens: 64,
                    tools: vec![],
                },
            }
        };
        let (tx, mut rx) = mpsc::channel(16);
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        };
        let (result, events) = tokio::join!(
            worker.invoke(operation, services.clone(), tx, &cancel),
            collect
        );
        result.unwrap();
        assert!(services.streams.lock().await.is_empty());
        if models {
            assert!(
                matches!(&events[..], [Event::Models { models }] if models[0].metadata.as_option().unwrap().context_window == Some(8192))
            );
        } else {
            assert!(
                matches!(&events[..], [Event::Delta { text }, Event::Finished { reason }] if text == "こんにちは" && reason == "stop")
            );
        }
    }
    server.abort();
}
