//! Plugin-provided tool definitions and invocation payloads (`F20`).
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(default = "default_schema")]
    pub parameters: Value,
    #[serde(default)]
    pub read_only: bool,
}

fn default_schema() -> Value {
    serde_json::json!({"type": "object"})
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvokeRequest {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvokeResult {
    pub output: Value,
    #[serde(default)]
    pub is_error: bool,
}
