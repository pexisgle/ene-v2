//! OpenRouter provider protocol v1. Only the host performs HTTP and attaches credentials.
use base64::{Engine, engine::general_purpose::STANDARD};
use ene_protocol::{
    ipc::{Reader, Writer},
    provider::{Capabilities, HostMessage, MAX_LINE, Model, ModelMetadata, PluginMessage, VERSION},
};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const HTTP_ID: &str = "http-1";
const MAX_MODELS: usize = 16 * 1024 * 1024;

struct Active {
    id: String,
    models: bool,
    source: String,
    pending: Vec<u8>,
    reason: Option<String>,
}

impl Active {
    fn response(&mut self, data: &[u8], done: bool) -> Result<Vec<PluginMessage>> {
        self.pending.extend_from_slice(data);
        let mut output = Vec::new();
        if self.models {
            if self.pending.len() > MAX_MODELS {
                return Err("model response limit".into());
            }
            if done {
                let value: Value = serde_json::from_slice(&self.pending)?;
                let models = value["data"]
                    .as_array()
                    .ok_or("missing model list")?
                    .iter()
                    .map(|m| {
                        let id = m["id"].as_str().ok_or("missing model id")?;
                        Ok(Model {
                            id: id.into(),
                            name: m["name"].as_str().unwrap_or(id).into(),
                            metadata: Some(ModelMetadata {
                                source: self.source.clone(),
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
                    .collect::<Result<Vec<_>>>()?;
                output.push(PluginMessage::Models {
                    request_id: self.id.clone(),
                    models,
                });
            }
        } else {
            // Keep bytes until a complete SSE line arrives; UTF-8 may span chunks.
            let mut consumed = 0;
            while let Some(end) = self.pending[consumed..].iter().position(|b| *b == b'\n') {
                let end = consumed + end;
                if end - consumed >= MAX_LINE {
                    return Err("SSE line limit".into());
                }
                Self::event(
                    &self.id,
                    &mut self.reason,
                    &self.pending[consumed..end],
                    &mut output,
                )?;
                consumed = end + 1;
            }
            self.pending.drain(..consumed);
            if self.pending.len() >= MAX_LINE {
                return Err("SSE line limit".into());
            }
            if done {
                Self::event(&self.id, &mut self.reason, &self.pending, &mut output)?;
                self.pending.clear();
                output.push(PluginMessage::Finished {
                    request_id: self.id.clone(),
                    reason: self.reason.clone().unwrap_or_else(|| "incomplete".into()),
                });
            }
        }
        Ok(output)
    }

    fn event(
        id: &str,
        reason: &mut Option<String>,
        line: &[u8],
        output: &mut Vec<PluginMessage>,
    ) -> Result<()> {
        let line = std::str::from_utf8(line)?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            return Ok(());
        };
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)?;
        if value.get("error").is_some() {
            return Err("upstream failure".into());
        }
        for choice in value["choices"].as_array().ok_or("missing choices")? {
            if let Some(text) = choice["delta"]["content"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                output.push(PluginMessage::Delta {
                    request_id: id.into(),
                    text: text.into(),
                });
            }
            if let Some(value) = choice["finish_reason"].as_str() {
                *reason = Some(value.into());
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Provider {
    active: Option<Active>,
}

impl Provider {
    fn handle(&mut self, message: HostMessage) -> Result<Vec<PluginMessage>> {
        match message {
            HostMessage::Initialize {
                request_id,
                protocol_version,
            } => {
                if protocol_version != VERSION {
                    return Err("unsupported protocol".into());
                }
                Ok(vec![PluginMessage::Initialized {
                    request_id,
                    protocol_version: VERSION,
                    capabilities: Capabilities {
                        streaming_text: true,
                        model_list: true,
                    },
                }])
            }
            HostMessage::Generate {
                request_id,
                request,
            } => {
                if request.options["upstream"] != "openai"
                    || request.options["allow_fallbacks"] != false
                {
                    return Err("this sample requires the approved OpenAI route".into());
                }
                let mut messages: Vec<Value> = request
                    .messages
                    .iter()
                    .map(|m| json!({"role": m.role, "content": m.text}))
                    .collect();
                if !request.preamble.is_empty() {
                    messages.insert(0, json!({"role":"system", "content":request.preamble}));
                }
                let body = json!({"model":request.model, "messages":messages, "stream":true,
                    "max_tokens":request.max_tokens, "provider":{"only":["openai"], "allow_fallbacks":false}});
                self.begin(
                    request_id,
                    &request.endpoint,
                    "/chat/completions",
                    Some(body),
                )
            }
            HostMessage::ListModels {
                request_id,
                endpoint,
                ..
            } => self.begin(request_id, &endpoint, "/models", None),
            HostMessage::HttpResponse {
                request_id,
                http_id,
                status,
                data_base64,
                done,
                ..
            } => {
                let Some(active) = &mut self.active else {
                    return Ok(Vec::new());
                };
                if active.id != request_id || http_id != HTTP_ID {
                    return Err("unknown HTTP context".into());
                }
                if !(200..300).contains(&status) {
                    return Err("HTTP failure".into());
                }
                let output = active.response(&STANDARD.decode(data_base64)?, done)?;
                if done {
                    self.active = None;
                }
                Ok(output)
            }
            HostMessage::Cancel { request_id } => {
                if self.active.as_ref().is_some_and(|a| a.id == request_id) {
                    self.active = None;
                }
                Ok(Vec::new())
            }
            HostMessage::Shutdown => Ok(Vec::new()),
        }
    }

    fn begin(
        &mut self,
        id: String,
        endpoint: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Vec<PluginMessage>> {
        if self.active.is_some() {
            return Err("busy".into());
        }
        let models = body.is_none();
        let body_base64 = body
            .map(|v| serde_json::to_vec(&v).map(|b| STANDARD.encode(b)))
            .transpose()?;
        let output = PluginMessage::HttpRequest {
            request_id: id.clone(),
            http_id: HTTP_ID.into(),
            method: if models { "GET" } else { "POST" }.into(),
            url: format!("{}{path}", endpoint.trim_end_matches('/')),
            headers: [
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "application/json".into()),
            ]
            .into(),
            body_base64,
        };
        self.active = Some(Active {
            id,
            models,
            source: format!("{}{path}", endpoint.trim_end_matches('/')),
            pending: Vec::new(),
            reason: None,
        });
        Ok(vec![output])
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut input = Reader::with_limit(tokio::io::stdin(), MAX_LINE);
    let mut output = Writer::with_limit(tokio::io::stdout(), MAX_LINE);
    let mut provider = Provider::default();
    while let Some(message) = input.receive::<HostMessage>().await? {
        let id = match &message {
            HostMessage::Shutdown => break,
            HostMessage::Initialize { request_id, .. }
            | HostMessage::ListModels { request_id, .. }
            | HostMessage::Generate { request_id, .. }
            | HostMessage::HttpResponse { request_id, .. }
            | HostMessage::Cancel { request_id } => request_id.clone(),
        };
        let messages = match provider.handle(message) {
            Ok(messages) => messages,
            Err(_) => {
                provider.active = None;
                vec![PluginMessage::Error {
                    request_id: id,
                    code: "provider_failure".into(),
                    message: "Provider request failed".into(),
                }]
            }
        };
        for message in messages {
            output.send(&message).await?;
        }
    }
    Ok(())
}
