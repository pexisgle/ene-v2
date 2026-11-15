//! LLM generation contract.
use crate::provider::{ChatMessage, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub preamble: String,
    pub messages: Vec<ChatMessage>,
    /// The tools the model may call. Empty means it may call none, and a
    /// plugin that returns a call anyway is refused.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmCapabilities {
    pub streaming_text: bool,
    #[serde(default)]
    pub model_list: bool,
    /// Whether the plugin can ask for tool calls. A plugin that cannot is
    /// offered no tools, so a host never waits on a call it will not get.
    #[serde(default)]
    pub tool_calls: bool,
}
