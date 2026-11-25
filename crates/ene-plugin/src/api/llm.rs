//! LLM capabilities.
use serde::{Deserialize, Serialize};

/// What an LLM provider supports beyond the request/response base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmCapabilities {
    /// Whether the provider streams its reply as it generates.
    pub streaming_text: bool,
    /// Whether the provider can list the models it serves.
    #[serde(default)]
    pub model_list: bool,
    /// Whether the plugin can ask for tool calls. A plugin that cannot is
    /// offered no tools, so a host never waits on a call it will not get.
    #[serde(default)]
    pub tool_calls: bool,
    /// Whether the plugin accepts `GenerateHttp`: an OpenAI-compatible HTTP
    /// request forwarded through the broker, answered with HTTP chunks. This
    /// is the only generation format an ABI 7 LLM provider may serve.
    #[serde(default)]
    pub openai_http: bool,
}
