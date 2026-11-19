//! Lifecycle hook events and decisions (`F18`).
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookEvent {
    PreTurn {
        companion_id: String,
        #[serde(default)]
        query: Option<String>,
    },
    PostTurn {
        companion_id: String,
        #[serde(default)]
        reply: Option<String>,
    },
    PreTool {
        companion_id: String,
        tool_name: String,
        arguments: Value,
    },
    PostTool {
        companion_id: String,
        tool_name: String,
        is_error: bool,
    },
    SessionStart {
        companion_id: String,
    },
    SessionEnd {
        companion_id: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookDecision {
    #[default]
    Continue,
    Block {
        reason: String,
    },
    InjectContext {
        text: String,
    },
}
