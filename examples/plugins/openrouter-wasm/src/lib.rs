//! Extism PDK example. HTTP and credentials remain owned by the ene broker.
//!
//! Generation is a raw OpenAI-compatible exchange: the host's rig wire builds
//! the request, this guest forwards it through the broker, and the host decodes
//! the reply. The guest does not parse SSE or assemble tool calls.
#[cfg(target_family = "wasm")]
mod guest {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ene_plugin::*;
    use serde_json::{Value, json};
    const MAX_RESPONSE: usize = 8 * 1024 * 1024;

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
            let closed = self.handle.take().map_or(Ok(()), http_close);
            result.and(closed)
        }
    }

    fn check_chunk(chunk: &HttpChunk) -> Result<usize, extism_pdk::Error> {
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
        Ok(STANDARD.decode(&chunk.data_base64)?.len())
    }

    /// Forwards the host's OpenAI-compatible request through the broker.
    /// Chunks go to the host verbatim; the host's rig wire decodes them.
    fn forward(request: HttpRequest) -> Result<(), extism_pdk::Error> {
        let mut response = BrokerResponse::open(request)?;
        let mut total = 0usize;
        loop {
            total += check_chunk(&response.chunk)?;
            if total > MAX_RESPONSE {
                return Err(extism_pdk::Error::msg("response limit"));
            }
            emit(Event::Http {
                chunk: response.chunk.clone(),
            })?;
            if response.chunk.done {
                break;
            }
            response.read()?;
        }
        response.finish(Ok(()))
    }

    /// Reads the provider's model list. This is a JSON document, not an SSE
    /// stream, so it stays a whole-body read.
    fn list_models(endpoint: String) -> Result<(), extism_pdk::Error> {
        let request = HttpRequest {
            method: "GET".into(),
            url: format!("{}/models?providers=openai", endpoint.trim_end_matches('/')),
            headers: std::collections::BTreeMap::new(),
            body_base64: None,
            stream: false,
        };
        let mut response = BrokerResponse::open(request)?;
        let mut pending = Vec::new();
        loop {
            if !(200..300).contains(&response.chunk.status) {
                return Err(extism_pdk::Error::msg("HTTP failure"));
            }
            STANDARD.decode_vec(&response.chunk.data_base64, &mut pending)?;
            if pending.len() > MAX_RESPONSE {
                return Err(extism_pdk::Error::msg("response limit"));
            }
            if response.chunk.done {
                break;
            }
            response.read()?;
        }
        response.finish(Ok(()))?;
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
                    },
                })
            })
            .collect::<Result<Vec<_>, extism_pdk::Error>>()?;
        emit(Event::Models { models })?;
        Ok(())
    }

    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<LlmCapabilities>> {
        Ok(Json(LlmCapabilities {
            streaming_text: true,
            model_list: true,
            tool_calls: true,
            openai_http: true,
        }))
    }

    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Operation>) -> FnResult<Json<Value>> {
        match operation {
            Operation::GenerateHttp { request } => forward(request)?,
            Operation::ListModels { endpoint, .. } => list_models(endpoint)?,
            _ => return Err(extism_pdk::Error::msg("unsupported operation").into()),
        }
        Ok(Json(json!({})))
    }
}
