//! Extism PDK example. HTTP and credentials remain owned by the ene broker.
#[cfg(target_family = "wasm")]
mod pending_calls;
#[cfg(target_family = "wasm")]
mod guest {
    use crate::pending_calls::{MAX_EVENT, PendingCalls};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ene_plugin::*;
    use serde_json::{Value, json};
    const MAX_RESPONSE: usize = 8 * 1024 * 1024;
    const MAX_LINE: usize = 1024 * 1024;

    struct BrokerResponse {
        chunk: HttpChunk,
        handle: Option<String>,
    }

    impl BrokerResponse {
        fn open(request: HttpRequest) -> Result<Self, extism_pdk::Error> {
            let chunk = http_open(request)?;
            let handle = (!chunk.done).then(|| chunk.handle.clone());
            Ok(Self { chunk, handle })
        }

        fn read(&mut self) -> Result<(), extism_pdk::Error> {
            let handle = self
                .handle
                .as_ref()
                .ok_or_else(|| extism_pdk::Error::msg("read after EOF"))?
                .clone();
            let chunk = http_read(handle.clone())?;
            if chunk.done {
                self.handle = None;
            }
            if chunk.handle != handle {
                return Err(extism_pdk::Error::msg("changed response handle"));
            }
            self.chunk = chunk;
            Ok(())
        }

        fn finish(
            mut self,
            result: Result<(), extism_pdk::Error>,
        ) -> Result<(), extism_pdk::Error> {
            let closed = self.handle.take().map(http_close).unwrap_or(Ok(()));
            result.and(closed)
        }
    }

    fn decode_response(
        response: &mut BrokerResponse,
        models: bool,
    ) -> Result<(), extism_pdk::Error> {
        let mut pending = Vec::new();
        let mut calls = PendingCalls::default();
        let mut terminal = false;
        'response: loop {
            let chunk = &response.chunk;
            if !(200..300).contains(&chunk.status) {
                return Err(extism_pdk::Error::msg("HTTP failure"));
            }
            if chunk.handle.len() > 1024
                || chunk.headers.len() > 64
                || chunk
                    .headers
                    .iter()
                    .map(|(key, value)| key.len().saturating_add(value.len()))
                    .sum::<usize>()
                    > 64 * 1024
            {
                return Err(extism_pdk::Error::msg("HTTP metadata limit"));
            }
            if chunk.data_base64.len() > MAX_RESPONSE.div_ceil(3) * 4 {
                return Err(extism_pdk::Error::msg("response limit"));
            }
            STANDARD.decode_vec(&chunk.data_base64, &mut pending)?;
            if pending.len() > MAX_RESPONSE {
                return Err(extism_pdk::Error::msg("response limit"));
            }
            if !models {
                let mut consumed = 0;
                while let Some(end) = pending[consumed..].iter().position(|b| *b == b'\n') {
                    if end > MAX_LINE {
                        return Err(extism_pdk::Error::msg("SSE line limit"));
                    }
                    let end = consumed + end;
                    let line = std::str::from_utf8(&pending[consumed..end])?;
                    if let Some(data) = line.strip_prefix("data:").map(str::trim)
                        && !data.is_empty()
                    {
                        if data == "[DONE]" {
                            terminal = true;
                            break 'response;
                        }
                        let value: Value = serde_json::from_str(data)?;
                        if value.get("error").is_some() {
                            return Err(extism_pdk::Error::msg("upstream failure"));
                        }
                        let choices = value["choices"]
                            .as_array()
                            .ok_or_else(|| extism_pdk::Error::msg("missing choices"))?;
                        for choice in choices {
                            if let Some(text) = choice["delta"]["content"].as_str() {
                                let event = Event::Delta { text: text.into() };
                                if serde_json::to_vec(&event)?.len() > MAX_EVENT {
                                    return Err(extism_pdk::Error::msg("delta event limit"));
                                }
                                emit(event)?;
                            }
                        }
                        calls.push(choices)?;
                    }
                    consumed = end + 1;
                }
                pending.drain(..consumed);
                if pending.len() > MAX_LINE {
                    return Err(extism_pdk::Error::msg("SSE line limit"));
                }
            }
            if chunk.done {
                if !models && calls.has_calls() && !pending.iter().all(u8::is_ascii_whitespace) {
                    return Err(extism_pdk::Error::msg("incomplete tool SSE line"));
                }
                break;
            }
            response.read()?;
        }
        if models {
            let value: Value = serde_json::from_slice(&pending)?;
            let models = value["data"]
                .as_array()
                .ok_or_else(|| extism_pdk::Error::msg("missing models"))?;
            if models.len() > 32768 {
                return Err(extism_pdk::Error::msg("model count limit"));
            }
            let models = models
                .iter()
                .map(|m| {
                    let id = m["id"]
                        .as_str()
                        .ok_or_else(|| extism_pdk::Error::msg("missing id"))?;
                    let name = m["name"].as_str().unwrap_or(id);
                    Ok(Model {
                        id: id.into(),
                        name: name.into(),
                        metadata: ModelMetadata {
                            source: "provider".into(),
                            scope: "model".into(),
                            context_window: m["context_length"].as_u64(),
                            max_output_tokens: m["top_provider"]["max_completion_tokens"].as_u64(),
                            ..Default::default()
                        }
                        .into(),
                        ..Default::default()
                    })
                })
                .collect::<Result<Vec<_>, extism_pdk::Error>>()?;
            let event = Event::Models { models };
            if serde_json::to_vec(&event)?.len() > MAX_EVENT {
                return Err(extism_pdk::Error::msg("model event limit"));
            }
            emit(event)?;
        } else {
            let (events, reason) = calls.finish(terminal)?;
            let finished = Event::Finished { reason };
            if serde_json::to_vec(&finished)?.len() > MAX_EVENT {
                return Err(extism_pdk::Error::msg("finish event limit"));
            }
            for event in events {
                emit(event)?;
            }
            emit(finished)?;
        }
        Ok(())
    }

    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<LlmCapabilities>> {
        Ok(Json(LlmCapabilities {
            streaming_text: true,
            model_list: true,
            tool_calls: true,
        }))
    }
    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Operation>) -> FnResult<Json<Value>> {
        let (endpoint, body, models) = match operation {
            Operation::Generate { request } => {
                let mut messages: Vec<Value> = Vec::new();
                for message in &request.messages {
                    match message {
                        ChatMessage::Text { role, text } => {
                            messages.push(json!({"role": role, "content": text}));
                        }
                        // The assistant turn that asked for the calls, in the
                        // shape an OpenAI-compatible endpoint replays.
                        ChatMessage::ToolCalls { calls } => messages.push(json!({
                            "role": "assistant",
                            "tool_calls": calls
                                .iter()
                                .map(|call| json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments.to_string(),
                                    },
                                }))
                                .collect::<Vec<_>>(),
                        })),
                        ChatMessage::ToolResult {
                            call_id, content, ..
                        } => messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": content,
                        })),
                    }
                }
                if !request.preamble.is_empty() {
                    messages.insert(0, json!({"role":"system","content":request.preamble}));
                }
                let mut body =
                    json!({"model":request.model,"stream":true,"max_tokens":request.max_tokens});
                body["messages"] = Value::Array(messages);
                if !request.tools.is_empty() {
                    body["tools"] = Value::Array(
                        request
                            .tools
                            .iter()
                            .map(|tool| {
                                json!({
                                    "type": "function",
                                    "function": {
                                        "name": tool.name,
                                        "description": tool.description,
                                        "parameters": tool.parameters,
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                (
                    format!(
                        "{}/chat/completions",
                        request.endpoint.trim_end_matches('/')
                    ),
                    Some(body),
                    false,
                )
            }
            // Match the single upstream approved by this sample's manifest.
            Operation::ListModels { endpoint, .. } => (
                format!("{}/models?providers=openai", endpoint.trim_end_matches('/')),
                None,
                true,
            ),
            _ => return Err(extism_pdk::Error::msg("unsupported operation").into()),
        };
        let request = HttpRequest {
            method: if models { "GET" } else { "POST" }.into(),
            url: endpoint,
            headers: std::collections::BTreeMap::from([(
                "content-type".into(),
                "application/json".into(),
            )]),
            body_base64: body.map(|v| STANDARD.encode(v.to_string())),
        };
        let mut response = BrokerResponse::open(request)?;
        let result = decode_response(&mut response, models);
        response.finish(result)?;
        Ok(Json(json!({})))
    }
}
