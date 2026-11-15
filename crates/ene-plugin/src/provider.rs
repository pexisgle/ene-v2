//! Shared LLM configuration and message types for Wasm plugins.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthProfile {
    None,
    Bearer,
    Header { name: String },
    Query { name: String },
}

// Reuse the shared data schema; plugin transport belongs to the runtime.
pub use ene_rpc::{LlmProviderCapabilities, Model, ModelMetadata, SettingField};

/// One turn of the conversation the plugin answers.
///
/// A turn is either text or a tool exchange. A ReAct loop needs both: the
/// assistant turn that asked for a call, and the turn that answers it, so the
/// model can continue from what the tool returned.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatMessage {
    /// A system, user or assistant turn carrying text.
    Text {
        /// `system`, `user` or `assistant`.
        role: String,
        text: String,
    },
    /// An assistant turn that asked for one or more tool calls.
    ToolCalls {
        /// The calls the model asked for, in the order it asked.
        calls: Vec<ToolCallRequest>,
    },
    /// The result of one call, as the host ran it.
    ToolResult {
        /// The call this answers.
        call_id: String,
        /// The tool that ran.
        name: String,
        /// What it returned, as text.
        content: String,
        /// Whether it failed; `content` then says why.
        #[serde(default)]
        is_error: bool,
    },
}

impl ChatMessage {
    /// A plain text turn.
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Text {
            role: role.into(),
            text: text.into(),
        }
    }
}

/// One call the model asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRequest {
    /// The call's id, which its result answers.
    pub id: String,
    /// The tool to run.
    pub name: String,
    /// The arguments as a JSON object.
    pub arguments: serde_json::Value,
}

/// One tool the model may call.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// Host-enforced intermediary routing; plugin-supplied HTTP bodies cannot override it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerRouting {
    #[default]
    Direct,
    OpenRouter {
        only: Vec<String>,
    },
}
