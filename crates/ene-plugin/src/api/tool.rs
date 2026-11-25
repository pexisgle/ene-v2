//! Plugin-provided tool definitions and invocation payloads (`F20`).
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool the plugin offers the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolDefinition {
    /// Tool name the model calls.
    pub name: String,
    /// What the tool does, written for the model.
    pub description: String,
    /// JSON Schema the host validates arguments against.
    #[serde(default = "default_schema")]
    pub parameters: Value,
    /// Whether calling it has no external effect.
    #[serde(default)]
    pub read_only: bool,
}

fn default_schema() -> Value {
    serde_json::json!({"type": "object"})
}

/// One tool call the host routed to the plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvokeRequest {
    /// The tool to run.
    pub name: String,
    /// Arguments, already validated against `parameters`.
    pub arguments: Value,
}

/// What the tool call produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvokeResult {
    /// The tool result, returned to the model as-is.
    pub output: Value,
    /// Whether `output` describes a failure rather than a result.
    #[serde(default)]
    pub is_error: bool,
}
