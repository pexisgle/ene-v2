use ene_ipc::{
    provider::{
        Client, Codec, GenerationRequest, HttpBroker, HttpBrokerServerShared, HttpRequest,
        HttpResponsePart, Provider, ProviderError, Sender,
    },
    remoc::{self, ConnectExt, rch::mpsc, rtc::ServerShared},
};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio_util::task::AbortOnDropHandle;

struct Broker;
impl HttpBroker for Broker {
    async fn request(
        &self,
        request: HttpRequest,
        out: Sender<HttpResponsePart>,
    ) -> Result<(), ProviderError> {
        let bytes = if request.method == "GET" {
            assert_eq!(
                request.url,
                "https://openrouter.ai/api/v1/models?providers=openai"
            );
            br#"{"data":[{"id":"openai/example","context_length":128000}]}"#.to_vec()
        } else {
            assert_eq!(request.url, "https://openrouter.ai/api/v1/chat/completions");
            let body: serde_json::Value = serde_json::from_slice(&request.body.unwrap()).unwrap();
            assert_eq!(
                body["provider"],
                serde_json::json!({"only":["openai"],"allow_fallbacks":false})
            );
            assert_eq!(body["messages"][0]["content"], "system");
            "data: {\"choices\":[{\"delta\":{\"content\":\"日本語\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n".as_bytes().to_vec()
        };
        out.send(HttpResponsePart::Head {
            status: 200,
            headers: Default::default(),
        })
        .await
        .map_err(|_| ProviderError::Unavailable)?
        .await
        .map_err(|_| ProviderError::Unavailable)?;
        for chunk in bytes.chunks(5) {
            out.send(HttpResponsePart::Data(chunk.to_vec()))
                .await
                .map_err(|_| ProviderError::Unavailable)?
                .await
                .map_err(|_| ProviderError::Unavailable)?;
        }
        Ok(())
    }
}
#[tokio::test]
async fn rust_provider_uses_rtc_and_http_streams_in_a_child_process() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ene-provider-openrouter"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client: Client = remoc::Connect::io::<_, _, Client, Client, remoc::codec::Default>(
            remoc::Cfg::default(),
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
        )
        .consume()
        .await
        .unwrap();
        let (server, broker) = HttpBrokerServerShared::<_, Codec>::new(Arc::new(Broker));
        let _serving = AbortOnDropHandle::new(tokio::spawn(server.serve()));
        assert!(client.capabilities().await.unwrap().streaming_text);
        let models = client
            .list_models(
                "https://openrouter.ai/api/v1".into(),
                serde_json::json!({}),
                broker.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            models[0].metadata.as_ref().unwrap().context_window,
            Some(128000)
        );
        let (sender, mut receiver) = mpsc::channel::<String, Codec>();
        let (reason, text) = tokio::join!(
            client.generate(
                GenerationRequest {
                    endpoint: "https://openrouter.ai/api/v1".into(),
                    options: serde_json::json!({"upstream":"openai","allow_fallbacks":false}),
                    model: "openai/example".into(),
                    preamble: "system".into(),
                    messages: vec![],
                    max_tokens: 1024,
                },
                broker,
                sender
            ),
            async {
                let mut text = String::new();
                while let Some(delta) = receiver.recv().await.unwrap() {
                    text.push_str(&delta);
                }
                text
            }
        );
        assert_eq!(reason.unwrap(), "stop");
        assert_eq!(text, "日本語");
        drop(client);
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}
