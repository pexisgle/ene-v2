//! LLM capabilities.
use serde::{Deserialize, Serialize};

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
