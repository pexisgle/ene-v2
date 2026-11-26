//! Companion avatar and voice configuration stored as schema-tagged JSON.

/// Structured payload for `Companion.body` (`schema = "ene.body.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BodyBinding {
    /// Avatar model to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Monitor the avatar appears on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    /// Screen position of the avatar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 2]>,
    /// Avatar size as a percentage of its natural size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_percent: Option<f32>,
}

/// Structured payload for `Companion.voice` (`schema = "ene.voice.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceBinding {
    /// The connection that synthesises the voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Provider voice id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
    /// Speaking rate multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    /// Pitch multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    /// Speaking style the provider understands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// Connection and model selected for one companion role.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelAssignment {
    /// The connection the model is reached through.
    pub connection_id: String,
    /// The model to call on that connection.
    pub model_id: String,
}

impl ModelAssignment {
    /// Returns whether both connection and model IDs are non-empty.
    pub fn is_set(&self) -> bool {
        !self.connection_id.is_empty() && !self.model_id.is_empty()
    }
}

/// The seven model roles from the contract. `companion` and `task` are
/// required to run anything; the rest fall back as documented.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelAssignments {
    /// The model that speaks as the companion.
    pub companion: ModelAssignment,
    /// The model that runs commissioned work.
    pub task: ModelAssignment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model that interprets perception input.
    pub perception: Option<ModelAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model that condenses conversation history.
    pub compaction: Option<ModelAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model that folds history into memory.
    pub consolidation: Option<ModelAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model that decides policy questions.
    pub decision: Option<ModelAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The model that produces embeddings.
    pub embedding: Option<ModelAssignment>,
}

impl ModelAssignments {
    /// Clears any role referencing `id` and returns whether any assignment changed.
    pub fn remove_connection(&mut self, id: &str) -> bool {
        let mut changed = false;
        for assignment in [&mut self.companion, &mut self.task] {
            if assignment.connection_id == id {
                *assignment = ModelAssignment::default();
                changed = true;
            }
        }
        for assignment in [
            &mut self.perception,
            &mut self.compaction,
            &mut self.consolidation,
            &mut self.decision,
            &mut self.embedding,
        ] {
            if assignment
                .as_ref()
                .is_some_and(|value| value.connection_id == id)
            {
                *assignment = None;
                changed = true;
            }
        }
        changed
    }
}
