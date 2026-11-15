//! A provider plugin as a rig completion model.
//!
//! The host keeps rig's catalog path for the providers rig ships. A provider
//! that is not in the catalog reaches the agent loop through the same
//! [`DynModel`] boundary: this wire encodes the request, and its transport runs
//! the plugin's `ene_invoke` and turns the plugin's events into the frames the
//! decoder reads.
//!
//! A plugin answers a generation with a stream of events rather than one
//! document, so this wire supplies both halves rig needs for a completion: a
//! decoder that writes the reply's parts, and a reassembler that folds the
//! events back into the provider document `raw` holds.
use super::super::providers::Running;
use ene_plugin::{
    Event as PluginEvent, GenerationRequest, Operation as PluginOperation, ToolCallRequest,
    ToolSpec, provider::ChatMessage,
};
use ene_plugin_runtime::instance::Services;
use rig_core::{
    DynModel,
    completion::{
        CompletionRequest, FinishReason, Message, Usage,
        message::{AssistantContent, ToolResultContent, UserContent},
    },
    driver::{Exchange, Opened, Opening, Transport},
    error::{EncodeError, ProviderError},
    operation::{Block, Completion, Finish},
    wire::{
        Descriptor, Flow, Mode, Out, Wire, WireEvent,
        document::{Reassemble, Serves},
    },
};
use std::sync::Arc;

/// One frame of a plugin's reply: an event it emitted, or the end of the turn.
#[derive(Clone, Debug)]
pub(crate) enum PluginFrame {
    /// A text fragment.
    Text(String),
    /// A tool call the model asked for.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// The turn ended with the plugin's reason.
    Finished(String),
}

/// The completion wire of one provider plugin.
#[derive(Clone)]
pub(crate) struct PluginCompletion {
    provider: Arc<Running>,
    model: String,
}

/// What the transport carries: the request the plugin answers.
pub(crate) struct CompletionPayload {
    request: GenerationRequest,
}

/// A plugin-backed completion model, erased to rig's operation.
pub(crate) fn completion(
    provider: Arc<Running>,
    model: String,
    broker: Arc<ene_plugin_runtime::service::HttpBroker>,
) -> DynModel<Completion> {
    let wire = PluginCompletion {
        provider: Arc::clone(&provider),
        model,
    };
    let transport = CompletionTransport { provider, broker };
    rig_core::Model::new(wire, transport).erase()
}

impl Wire for PluginCompletion {
    type Op = Completion;
    type Payload = CompletionPayload;
    type Frame = PluginFrame;
    type Event = PluginFrame;
    type Decoder<'id> = PluginDecoder;
    type Reassembler = PluginDocument;

    fn describe(&self) -> Descriptor<'_> {
        Descriptor::new(self.provider.provider().id.as_str())
            .model(self.model.as_str())
            .replay(self)
    }

    fn encode(
        &self,
        request: CompletionRequest,
        _mode: Mode,
    ) -> Result<CompletionPayload, EncodeError> {
        use rig_core::completion::message::ToolChoice;
        if let Some((field, _)) = [
            ("temperature", request.temperature.is_some()),
            (
                "tool_choice",
                !matches!(request.tool_choice.as_ref(), None | Some(ToolChoice::Auto)),
            ),
            ("output_schema", request.output_schema.is_some()),
            ("additional_params", request.additional_params.is_some()),
            ("provider_options", !request.provider_options.is_empty()),
        ]
        .into_iter()
        .find(|(_, set)| *set)
        {
            return Err(EncodeError::request(format!(
                "the plugin generation contract cannot represent `{field}`"
            )));
        }
        let mut messages = Vec::new();
        for message in &request.chat_history {
            match message.as_ref() {
                Message::System { content } => {
                    if !content.is_empty() {
                        messages.push(ChatMessage::text("system", content.clone()));
                    }
                }
                Message::User { content } => {
                    for item in content {
                        match item {
                            UserContent::Text(text) if !text.text.is_empty() => {
                                messages.push(ChatMessage::text("user", text.text.clone()));
                            }
                            // A tool's answer is its own turn, so the model
                            // can tell it apart from something the user said.
                            UserContent::ToolResult(result) => {
                                messages.push(ChatMessage::ToolResult {
                                    call_id: result.call.wire().into_owned(),
                                    name: result.name.to_string(),
                                    content: result_text(&result.content),
                                    is_error: result.is_error,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Message::Assistant(turn) => {
                    let text = turn.text();
                    if !text.is_empty() {
                        messages.push(ChatMessage::text("assistant", text));
                    }
                    // The calls the assistant asked for are their own turn,
                    // ahead of the results that answer them.
                    let calls: Vec<ToolCallRequest> = turn
                        .content
                        .iter()
                        .filter_map(|item| match item {
                            AssistantContent::ToolCall(call) => Some(ToolCallRequest {
                                id: call.id.wire().into_owned(),
                                name: call.function.name.to_string(),
                                arguments: serde_json::Value::Object(
                                    call.function.arguments.clone(),
                                ),
                            }),
                            _ => None,
                        })
                        .collect();
                    if !calls.is_empty() {
                        messages.push(ChatMessage::ToolCalls { calls });
                    }
                }
            }
        }
        Ok(CompletionPayload {
            request: GenerationRequest {
                endpoint: self.provider.provider().endpoint.clone(),
                options: self.provider.provider().options.clone(),
                model: request.model.unwrap_or_else(|| self.model.clone()),
                preamble: String::new(),
                messages,
                tools: request
                    .tools
                    .iter()
                    .map(|tool| ToolSpec {
                        name: tool.name.to_string(),
                        description: tool.description.clone(),
                        parameters: tool.parameters.clone(),
                    })
                    .collect(),
                max_tokens: request.max_tokens.unwrap_or(4096),
            },
        })
    }

    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        PluginDecoder::default()
    }

    fn reassembler(&self) -> Self::Reassembler {
        PluginDocument::default()
    }
}

impl std::fmt::Debug for PluginCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginCompletion")
            .field("provider", &self.provider.provider().id)
            .field("model", &self.model)
            .finish()
    }
}

impl rig_core::completion::ReplayTarget for PluginCompletion {
    fn api(&self) -> rig_core::message::Api {
        rig_core::message::Api::from_static("ene.plugin")
    }

    fn provider(&self) -> &str {
        &self.provider.provider().id
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn accepts(&self, _model: &str) -> rig_core::completion::history::Accepts {
        rig_core::completion::history::Accepts::TEXT
    }

    fn encodes(&self, _model: &str, _media: rig_core::completion::history::Media<'_>) -> bool {
        false
    }

    fn catalog_priced(&self) -> bool {
        false
    }

    fn map_options(
        &self,
        _request: &CompletionRequest,
        fields: rig_core::completion::options::OptionFields<'_>,
    ) -> rig_core::completion::options::OptionMap {
        use rig_core::completion::options::{Mapping, OptionFields, OptionMap};
        let OptionFields {
            reasoning,
            cache,
            service_tier,
            verbosity,
            parallel_tool_calls,
            top_p,
            seed,
            stop,
            cache_key,
        } = fields;
        let unsupported = |set| {
            if set {
                Mapping::unsupported("the plugin generation contract has no such option")
            } else {
                Mapping::Nothing
            }
        };
        OptionMap {
            reasoning: unsupported(reasoning.is_some()),
            cache: unsupported(cache.is_some()),
            service_tier: unsupported(service_tier.is_some()),
            verbosity: unsupported(verbosity.is_some()),
            parallel_tool_calls: unsupported(parallel_tool_calls.is_some()),
            top_p: unsupported(top_p.is_some()),
            seed: unsupported(seed.is_some()),
            stop: unsupported(!stop.is_empty()),
            cache_key: Mapping::unrouted(cache_key),
        }
    }
}

/// What a tool returned, as the one text a plugin's contract carries. A
/// structured result is serialized, so the model sees what the tool said
/// rather than a rendering of it.
fn result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Json { value } => value.to_string(),
            ToolResultContent::Image(image) => {
                format!(
                    "[image: {}]",
                    image
                        .media_type
                        .as_ref()
                        .map_or("unknown", |kind| match kind {
                            rig_core::completion::message::ImageMediaType::JPEG => "jpeg",
                            kind => kind.extension(),
                        })
                )
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Writes the reply's parts from the plugin's events.
#[derive(Default)]
pub(crate) struct PluginDecoder {
    /// The index of the open text part, once one opened.
    text: Option<usize>,
    /// Whether the plugin ended the turn.
    ended: bool,
    reason: Option<String>,
}

impl<'id> rig_core::wire::Decoder<'id, Completion, PluginFrame> for PluginDecoder {
    type Event = PluginFrame;

    fn classify(&self, frame: PluginFrame) -> WireEvent<PluginFrame> {
        WireEvent::Known(frame)
    }

    fn decode(
        &mut self,
        event: PluginFrame,
        mut out: Out<'id, Completion>,
    ) -> Result<Flow, ProviderError> {
        match event {
            PluginFrame::Text(text) => {
                // One text part for the whole answer: a plugin streams
                // fragments of one block, not several.
                let index = match self.text {
                    Some(index) => index,
                    None => {
                        let index = out.fresh_index();
                        out.open(index, Block::Text, serde_json::Value::Null)?;
                        self.text = Some(index);
                        index
                    }
                };
                out.push(index, &text)?;
                Ok(Flow::More)
            }
            PluginFrame::ToolCall {
                id,
                name,
                arguments,
            } => {
                // A call is one whole block: the plugin states its arguments
                // in one event, so there is nothing to stream into it.
                let index = out.fresh_index();
                let call_id = rig_core::completion::message::CallId::from_wire(id);
                let name = rig_core::completion::message::ToolName::new(name)
                    .map_err(|_| ProviderError::Response("a tool call named no tool".into()))?;
                let body =
                    serde_json::Value::Object(arguments.as_object().cloned().unwrap_or_default());
                out.whole(
                    index,
                    Block::Call { id: call_id, name },
                    body,
                    &arguments.to_string(),
                )?;
                Ok(Flow::More)
            }
            PluginFrame::Finished(reason) => {
                self.ended = true;
                self.reason = Some(reason);
                if let Some(index) = self.text.take() {
                    out.finish(index)?;
                }
                Ok(out.end(Finish {
                    usage: Usage::default(),
                    reason: Some(finish_reason(self.reason.as_deref())),
                    response_id: None,
                    model: None,
                    error: None,
                }))
            }
        }
    }

    fn eof(&mut self, out: Out<'id, Completion>) -> Result<Flow, ProviderError> {
        // A plugin that stopped without its `Finished` event was cut short.
        if !self.ended {
            return Err(ProviderError::Truncated);
        }
        Ok(out.end(Finish {
            usage: Usage::default(),
            reason: Some(finish_reason(self.reason.as_deref())),
            response_id: None,
            model: None,
            error: None,
        }))
    }
}

/// The normalized reason a plugin's own word maps to.
fn finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") | Some("end_turn") | None => FinishReason::Stop,
        Some("length") | Some("max_tokens") => FinishReason::Length,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_owned()),
    }
}

/// Folds the reply's events into the provider document `raw` holds.
///
/// A plugin's frames carry text, the calls it asked for and an end, so the
/// document is the answer's text, its calls and the reason it stopped. It is
/// not a provider envelope: the plugin is the provider, and this is what it
/// said.
#[derive(Default)]
pub(crate) struct PluginDocument {
    text: String,
    calls: Vec<serde_json::Value>,
    reason: Option<String>,
}

impl Reassemble<PluginFrame> for PluginDocument {
    fn absorb(&mut self, event: &PluginFrame) {
        match event {
            PluginFrame::Text(text) => self.text.push_str(text),
            PluginFrame::ToolCall {
                id,
                name,
                arguments,
            } => self.calls.push(serde_json::json!({
                "id": id,
                "name": name,
                "arguments": arguments,
            })),
            PluginFrame::Finished(reason) => self.reason = Some(reason.clone()),
        }
    }

    fn finish(self) -> serde_json::Value {
        serde_json::json!({
            "text": self.text,
            "calls": self.calls,
            "reason": self.reason,
        })
    }
}

impl Serves<Completion> for PluginDocument {}

/// Runs one generation on the plugin and turns its events into frames.
#[derive(Clone)]
pub(crate) struct CompletionTransport {
    provider: Arc<Running>,
    broker: Arc<ene_plugin_runtime::service::HttpBroker>,
}

impl Transport<PluginCompletion> for CompletionTransport {
    fn send(&self, payload: CompletionPayload, _exchange: Exchange) -> Opening<PluginFrame> {
        let provider = Arc::clone(&self.provider);
        let broker = Arc::clone(&self.broker);
        Opening::new(async move {
            let request = payload.request;
            // The plugin's events arrive on a channel while it runs, so a
            // streamed answer reaches the caller as it is produced rather
            // than after the whole turn.
            let (sender, mut events) = tokio::sync::mpsc::channel(64);
            let services: Arc<dyn Services> = super::super::providers::BrokerServices::new(
                Arc::clone(&broker),
                provider.provider().id.clone(),
            );
            let running = tokio::spawn(async move {
                provider
                    .invoke_streaming(PluginOperation::Generate { request }, services, sender)
                    .await
            });
            let frames = async_stream::try_stream! {
                let mut terminal = None;
                while let Some(event) = events.recv().await {
                    match event {
                        PluginEvent::Delta { text } => yield PluginFrame::Text(text),
                        PluginEvent::ToolCall { id, name, arguments } => {
                            yield PluginFrame::ToolCall { id, name, arguments };
                        }
                        PluginEvent::Finished { reason } => {
                            terminal = Some(reason);
                            break;
                        }
                        _ => {}
                    }
                }
                // Keep the receiver open until the runtime's final checks finish.
                running.await
                    .map_err(|error| ProviderError::Response(error.to_string()))?
                    .map_err(|error| ProviderError::Response(error.to_string()))?;
                drop(events);
                if let Some(reason) = terminal {
                    yield PluginFrame::Finished(reason);
                }
            };
            Ok(Opened::new(frames))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::result_text;
    use rig_core::completion::message::{
        DocumentSourceKind, Image, ImageMediaType, ToolResultContent,
    };

    #[test]
    fn tool_results_preserve_text_and_legacy_image_labels() {
        let mut content = vec![
            ToolResultContent::text("tool"),
            ToolResultContent::Json {
                value: serde_json::json!({"ok": true}),
            },
        ];
        content.extend(
            [Some(ImageMediaType::JPEG), Some(ImageMediaType::PNG), None].map(|media_type| {
                ToolResultContent::Image(Image {
                    data: DocumentSourceKind::Raw(vec![1, 2, 3]),
                    media_type,
                    ..Default::default()
                })
            }),
        );
        assert_eq!(
            result_text(&content),
            "tool{\"ok\":true}[image: jpeg][image: png][image: unknown]"
        );
    }
}
