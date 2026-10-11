//! OpenRouter RTC provider. Only the host performs HTTP and attaches credentials.
use ene_ipc::{
    provider::{
        BrokerClient, Capabilities, Codec, GenerationRequest, HttpBroker, HttpRequest,
        HttpResponsePart, Model, ModelMetadata, Provider, ProviderError, ProviderServerShared,
        Sender,
    },
    remoc::{self, ConnectExt, rch::mpsc, rtc::ServerShared},
};
use eventsource_stream::Eventsource;
use futures_util::{FutureExt, StreamExt, stream::BoxStream};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::fd::AsFd;
use std::sync::Arc;
#[cfg(unix)]
use tokio::net::unix::pipe;

type Result<T> = std::result::Result<T, ProviderError>;

struct Sse {
    input: Option<
        futures_channel::mpsc::UnboundedSender<std::result::Result<Vec<u8>, std::io::Error>>,
    >,
    events: BoxStream<
        'static,
        std::result::Result<
            eventsource_stream::Event,
            eventsource_stream::EventStreamError<std::io::Error>,
        >,
    >,
    reason: Option<String>,
}
impl Default for Sse {
    fn default() -> Self {
        let (input, source) = futures_channel::mpsc::unbounded();
        Self {
            input: Some(input),
            events: source.eventsource().boxed(),
            reason: None,
        }
    }
}
impl Sse {
    fn response(&mut self, data: &[u8], done: bool) -> Result<Vec<String>> {
        self.input
            .as_ref()
            .ok_or(ProviderError::Failed)?
            .unbounded_send(Ok(data.to_vec()))
            .map_err(|_| ProviderError::Failed)?;
        if done {
            self.input.take();
        }
        let mut output = Vec::new();
        while let Some(Some(event)) = self.events.next().now_or_never() {
            let event = event.map_err(|_| ProviderError::Failed)?;
            Self::event(&mut self.reason, &event.data, &mut output)?;
        }
        Ok(output)
    }
    fn event(reason: &mut Option<String>, data: &str, output: &mut Vec<String>) -> Result<()> {
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).map_err(|_| ProviderError::Failed)?;
        if value.get("error").is_some() {
            return Err(ProviderError::Failed);
        }
        for choice in value["choices"].as_array().ok_or(ProviderError::Failed)? {
            if let Some(text) = choice["delta"]["content"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                output.push(text.into());
            }
            if let Some(value) = choice["finish_reason"].as_str() {
                *reason = Some(value.into());
            }
        }
        Ok(())
    }
}

fn models(source: &str, bytes: &[u8]) -> Result<Vec<Model>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ProviderError::Failed)?;
    value["data"]
        .as_array()
        .ok_or(ProviderError::Failed)?
        .iter()
        .map(|m| {
            let id = m["id"].as_str().ok_or(ProviderError::Failed)?;
            Ok(Model {
                id: id.into(),
                name: m["name"].as_str().unwrap_or(id).into(),
                metadata: Some(ModelMetadata {
                    source: source.into(),
                    context_window: m["context_length"].as_u64().filter(|v| *v > 0),
                    max_output_tokens: m["top_provider"]["max_completion_tokens"]
                        .as_u64()
                        .filter(|v| *v > 0),
                    input_modalities: strings(&m["architecture"]["input_modalities"]),
                    output_modalities: strings(&m["architecture"]["output_modalities"]),
                    supported_parameters: strings(&m["supported_parameters"]),
                    ..Default::default()
                }),
            })
        })
        .collect()
}
fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}
fn request(endpoint: &str, path: &str, body: Option<Value>) -> Result<HttpRequest> {
    Ok(HttpRequest {
        method: if body.is_some() { "POST" } else { "GET" }.into(),
        url: format!("{}{path}", endpoint.trim_end_matches('/')),
        headers: [
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                if body.is_some() {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .into(),
            ),
        ]
        .into(),
        body: body
            .map(|v| serde_json::to_vec(&v).map_err(|_| ProviderError::InvalidInput))
            .transpose()?,
    })
}
fn valid_head(part: HttpResponsePart) -> Result<Option<Vec<u8>>> {
    match part {
        HttpResponsePart::Head { status, .. } if (200..300).contains(&status) => Ok(None),
        HttpResponsePart::Head { .. } => Err(ProviderError::Failed),
        HttpResponsePart::Data(bytes) => Ok(Some(bytes)),
    }
}

struct OpenRouter;
impl Provider for OpenRouter {
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities {
            streaming_text: true,
            model_list: true,
        })
    }
    async fn list_models(
        &self,
        endpoint: String,
        _options: Value,
        broker: BrokerClient,
    ) -> Result<Vec<Model>> {
        let request = request(&endpoint, "/models?providers=openai", None)?;
        let source = request.url.clone();
        let (sender, mut receiver) = mpsc::channel::<HttpResponsePart, Codec>();
        let receive = async {
            let mut bytes = Vec::new();
            while let Some(part) = receiver
                .recv()
                .await
                .map_err(|_| ProviderError::Unavailable)?
            {
                if let Some(data) = valid_head(part)? {
                    bytes.extend(data);
                }
            }
            models(&source, &bytes)
        };
        let (_, models) = tokio::try_join!(broker.request(request, sender), receive)?;
        Ok(models)
    }
    async fn generate(
        &self,
        input: GenerationRequest,
        broker: BrokerClient,
        deltas: Sender<String>,
    ) -> Result<String> {
        if input.options["upstream"] != "openai" || input.options["allow_fallbacks"] != false {
            return Err(ProviderError::InvalidInput);
        }
        let mut messages: Vec<Value> = input
            .messages
            .iter()
            .map(|m| json!({"role":m.role,"content":m.text}))
            .collect();
        if !input.preamble.is_empty() {
            messages.insert(0, json!({"role":"system","content":input.preamble}));
        }
        let body = json!({"model":input.model,"messages":messages,"stream":true,"max_tokens":input.max_tokens,
            "provider":{"only":["openai"],"allow_fallbacks":false}});
        let request = request(&input.endpoint, "/chat/completions", Some(body))?;
        let (sender, mut receiver) = mpsc::channel::<HttpResponsePart, Codec>();
        let receive = async {
            let mut sse = Sse::default();
            while let Some(part) = receiver
                .recv()
                .await
                .map_err(|_| ProviderError::Unavailable)?
            {
                if let Some(bytes) = valid_head(part)? {
                    for text in sse.response(&bytes, false)? {
                        deltas
                            .send(text)
                            .await
                            .map_err(|_| ProviderError::Unavailable)?
                            .await
                            .map_err(|_| ProviderError::Unavailable)?;
                    }
                }
            }
            for text in sse.response(&[], true)? {
                deltas
                    .send(text)
                    .await
                    .map_err(|_| ProviderError::Unavailable)?
                    .await
                    .map_err(|_| ProviderError::Unavailable)?;
            }
            Ok(sse.reason.unwrap_or_else(|| "incomplete".into()))
        };
        let (_, reason) = tokio::try_join!(broker.request(request, sender), receive)?;
        Ok(reason)
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve());
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
async fn serve() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (server, client) = ProviderServerShared::<_, Codec>::new(Arc::new(OpenRouter));
    #[cfg(unix)]
    let input = pipe::Receiver::from_owned_fd(std::io::stdin().as_fd().try_clone_to_owned()?)?;
    #[cfg(unix)]
    let output = pipe::Sender::from_owned_fd(std::io::stdout().as_fd().try_clone_to_owned()?)?;
    #[cfg(windows)]
    let (input, output) = (tokio::io::stdin(), tokio::io::stdout());
    remoc::Connect::io::<_, _, _, _, remoc::codec::Default>(remoc::Cfg::default(), input, output)
        .provide(client)
        .await?;
    server.serve().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_uses_the_generation_route_and_normalizes_metadata() {
        let request = request(
            "https://openrouter.ai/api/v1/",
            "/models?providers=openai",
            None,
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://openrouter.ai/api/v1/models?providers=openai"
        );
        assert_eq!(request.method, "GET");
        assert!(request.body.is_none());
        let models = models(
            &request.url,
            br#"{"data":[{"id":"openai/gpt-4o","context_length":128000}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].metadata.as_ref().unwrap().source, request.url);
        assert_eq!(
            models[0].metadata.as_ref().unwrap().context_window,
            Some(128000)
        );
    }
    #[test]
    fn sse_preserves_split_utf8_and_last_line() {
        let mut sse = Sse::default();
        let bytes = "data: {\"choices\":[{\"delta\":{\"content\":\"日本語\"}}]}\r\n\r\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n".as_bytes();
        let mut text = String::new();
        for chunk in bytes.chunks(1) {
            text.extend(sse.response(chunk, false).unwrap());
        }
        text.extend(sse.response(&[], true).unwrap());
        assert_eq!(text, "日本語");
        assert_eq!(sse.reason.as_deref(), Some("stop"));
        assert!(
            Sse::default()
                .response(b"data: {\"error\":{}}\n\n", false)
                .is_err()
        );
    }
    #[test]
    fn multiline_sse_events_support_lf_crlf_and_cr() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = [
                ": comment",
                "data: {\"choices\":[",
                "data: {\"delta\":{\"content\":\"日本🙂\"},\"finish_reason\":\"stop\"}]}",
                "",
                "data: [DONE]",
                "",
                "",
            ]
            .join(ending);
            let mut sse = Sse::default();
            let mut text = String::new();
            for byte in input.as_bytes() {
                text.extend(sse.response(&[*byte], false).unwrap());
            }
            text.extend(sse.response(&[], true).unwrap());
            assert_eq!(text, "日本🙂");
            assert_eq!(sse.reason.as_deref(), Some("stop"));
        }
    }
}
