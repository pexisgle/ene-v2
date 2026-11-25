//! Character events and actions.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Something that happened, which the character plugin may react to.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterEvent {
    /// The character became the active one.
    Activated,
    /// The character is no longer active.
    Deactivated,
    /// The person started talking to the character.
    ConversationStarted,
    /// The conversation ended.
    ConversationFinished {
        /// The closing exchange, as the host summarised it.
        text: String,
    },
    /// The character's current task changed.
    TaskChanged {
        /// The task the character is now on.
        task_id: String,
        /// The task's state, in the host's own spelling.
        state: String,
    },
    /// A timer the character set has fired.
    Timer {
        /// Id of the timer that fired.
        id: String,
    },
    /// An event defined by the plugin itself.
    Custom {
        /// Event name the plugin recognises.
        name: String,
        /// Plugin-defined payload.
        data: Value,
    },
}

/// Something the character should do.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharacterAction {
    /// Say the given text.
    Speak {
        /// The words to speak.
        text: String,
    },
    /// Hold a facial expression.
    Expression {
        /// Expression name the renderer knows.
        name: String,
        /// How long to hold it, in milliseconds.
        duration_ms: u32,
    },
    /// Play a motion.
    Motion {
        /// Asset path inside the verified manifest.
        asset: String,
        /// How long the motion lasts, in milliseconds.
        duration_ms: u32,
    },
}

/// Explicit routing target; the host must authorize it against the binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterTarget {
    /// The device the character runs on.
    pub device_id: String,
    /// The character to address on that device.
    pub character_id: String,
}
