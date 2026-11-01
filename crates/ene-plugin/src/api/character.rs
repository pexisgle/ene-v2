//! Character events and actions.
use crate::WindowEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterEvent {
    Activated,
    Deactivated,
    ConversationStarted,
    ConversationFinished { text: String },
    TaskChanged { task_id: String, state: String },
    Timer { id: String },
    Window { event: WindowEvent },
    Custom { name: String, data: Value },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterAction {
    Speak { text: String },
    Expression { name: String, duration_ms: u32 },
    Motion { asset: String, duration_ms: u32 },
}

/// Explicit routing target; the host must authorize it against the binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterTarget {
    pub device_id: String,
    pub character_id: String,
}
