//! Lifecycle hook events and decisions (`F18`).
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A lifecycle moment the host reports to a hook plugin.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookEvent {
    /// A turn is about to start.
    PreTurn {
        /// The companion the turn belongs to.
        companion_id: String,
        /// What the person said, when the host passes it.
        #[serde(default)]
        query: Option<String>,
    },
    /// A turn has finished.
    PostTurn {
        /// The companion the turn belongs to.
        companion_id: String,
        /// What the companion replied, when the host passes it.
        #[serde(default)]
        reply: Option<String>,
    },
    /// A tool is about to run.
    PreTool {
        /// The companion the turn belongs to.
        companion_id: String,
        /// The tool being called.
        tool_name: String,
        /// The arguments the tool would receive.
        arguments: Value,
    },
    /// A tool has returned.
    PostTool {
        /// The companion the turn belongs to.
        companion_id: String,
        /// The tool that ran.
        tool_name: String,
        /// Whether the call failed.
        is_error: bool,
    },
    /// A session opened.
    SessionStart {
        /// The companion the session belongs to.
        companion_id: String,
    },
    /// A session closed.
    SessionEnd {
        /// The companion the session belongs to.
        companion_id: String,
    },
}

/// What a hook plugin wants the host to do next.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookDecision {
    /// Let the host carry on unchanged.
    #[default]
    Continue,
    /// Stop what the host was about to do.
    Block {
        /// Why it was blocked, shown to the operator.
        reason: String,
    },
    /// Add text to what the model sees.
    InjectContext {
        /// The text to add.
        text: String,
    },
}
