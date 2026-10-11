//! Extism PDK example. HTTP and credentials remain owned by the ene broker.
#[cfg(target_family = "wasm")]
#[allow(unused_braces)]
mod guest {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ene_ipc::provider::Model;
    use ene_plugin_sdk::*;
    use serde_json::{Value, json};
    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<Value>> {
        Ok(Json(json!({"streaming_text":true,"model_list":true})))
    }
    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Operation>) -> FnResult<Json<Value>> {
        let (endpoint, body, models) = match operation {
            Operation::Generate { request } => {
                if !request.tools.is_empty() {
                    return Err(extism_pdk::Error::msg("sample does not support tools").into());
                }
                let mut messages: Vec<Value> = request
                    .messages
                    .iter()
                    .map(|m| json!({"role":m.role,"content":m.text}))
                    .collect();
                if !request.preamble.is_empty() {
                    messages.insert(0, json!({"role":"system","content":request.preamble}));
                }
                (
                    format!(
                        "{}/chat/completions",
                        request.endpoint.trim_end_matches('/')
                    ),
                    Some(
                        json!({"model":request.model,"messages":messages,"stream":true,"max_tokens":request.max_tokens}),
                    ),
                    false,
                )
            }
            Operation::ListModels { endpoint, .. } => (
                format!("{}/models", endpoint.trim_end_matches('/')),
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
        let mut chunk = http_open(request)?;
        let mut pending = Vec::new();
        let mut reason = None;
        loop {
            if !(200..300).contains(&chunk.status) {
                return Err(extism_pdk::Error::msg("HTTP failure").into());
            }
            pending.extend(STANDARD.decode(&chunk.data_base64)?);
            if pending.len() > 8 * 1024 * 1024 {
                return Err(extism_pdk::Error::msg("response limit").into());
            }
            if !models {
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line = std::str::from_utf8(&pending[..end])?;
                    if let Some(data) = line.strip_prefix("data:").map(str::trim)
                        && !data.is_empty()
                        && data != "[DONE]"
                    {
                        let value: Value = serde_json::from_str(data)?;
                        if value.get("error").is_some() {
                            return Err(extism_pdk::Error::msg("upstream failure").into());
                        }
                        for choice in value["choices"]
                            .as_array()
                            .ok_or_else(|| extism_pdk::Error::msg("missing choices"))?
                        {
                            if let Some(text) = choice["delta"]["content"].as_str() {
                                emit(Event::Delta { text: text.into() })?;
                            }
                            if let Some(value) = choice["finish_reason"].as_str() {
                                reason = Some(value.to_string());
                            }
                        }
                    }
                    pending.drain(..=end);
                }
                if pending.len() > MAX_MESSAGE_BYTES {
                    return Err(extism_pdk::Error::msg("SSE line limit").into());
                }
            }
            if chunk.done {
                break;
            }
            chunk = http_read(chunk.handle)?;
        }
        if models {
            let value: Value = serde_json::from_slice(&pending)?;
            let models = value["data"]
                .as_array()
                .ok_or_else(|| extism_pdk::Error::msg("missing models"))?
                .iter()
                .map(|m| {
                    let id = m["id"]
                        .as_str()
                        .ok_or_else(|| extism_pdk::Error::msg("missing id"))?;
                    Ok(Model {
                        id: id.into(),
                        name: m["name"].as_str().unwrap_or(id).into(),
                        metadata: None,
                    })
                })
                .collect::<Result<Vec<_>, extism_pdk::Error>>()?;
            emit(Event::Models { models })?;
        } else {
            emit(Event::Finished {
                reason: reason.unwrap_or_else(|| "incomplete".into()),
            })?;
        }
        Ok(Json(json!({})))
    }
}
