use serde::{Deserialize, Serialize};

/// A lifecycle notification sent to an enabled host plugin.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookEvent {
    /// An opted-in sensor report before model evaluation.
    Perception {
        /// The device's observation.
        observation: String,
        /// The device's interpretation, if available.
        interpretation: String,
    },
    /// Speech has been admitted for synthesis.
    PreSpeech {
        /// Companion owning the utterance.
        companion_id: String,
    },
    /// The host finished producing audio for the utterance.
    PostSpeech {
        /// Companion owning the utterance.
        companion_id: String,
    },
    /// A work tool was requested.
    PreTool {
        /// Companion owning the work.
        companion_id: String,
        /// Registered tool name.
        tool_name: String,
    },
    /// A work tool returned or was refused.
    PostTool {
        /// Companion owning the work.
        companion_id: String,
        /// Registered tool name.
        tool_name: String,
        /// Whether the invocation failed or was refused.
        is_error: bool,
    },
}

/// Changes accepted only at the corresponding perception or speech boundary.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookDecision {
    /// Finish observing the notification.
    #[default]
    Continue,
    /// Drop selected fields before model evaluation.
    MaskPerception {
        /// Drop the observation field.
        observation: bool,
        /// Drop the interpretation field.
        interpretation: bool,
    },
    /// Adjust the speaking rate for the utterance.
    TuneSpeech {
        /// Rate multiplier between 0.5 and 2.
        speed: f32,
    },
}
