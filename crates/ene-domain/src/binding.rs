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

/// One of the companion's model roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRole {
    /// Speaks as the companion.
    Companion,
    /// Runs commissioned work.
    Task,
    /// Interprets screens and other perception input.
    Perception,
    /// Condenses conversation history.
    Compaction,
    /// Folds history into memory.
    Consolidation,
    /// Decides policy questions.
    Decision,
    /// Produces embeddings.
    Embedding,
}

impl ModelRole {
    /// Roles in the order presented in settings.
    pub const ALL: [Self; 7] = [
        Self::Companion,
        Self::Task,
        Self::Perception,
        Self::Compaction,
        Self::Consolidation,
        Self::Decision,
        Self::Embedding,
    ];

    /// Role to use when an optional assignment is absent.
    pub const fn fallback(self) -> Option<Self> {
        match self {
            Self::Perception | Self::Compaction | Self::Consolidation => Some(Self::Companion),
            Self::Decision => Some(Self::Task),
            Self::Companion | Self::Task | Self::Embedding => None,
        }
    }
}

/// The serialized assignments for the companion's model roles.
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
    /// The explicit selection, if both its connection and model are set.
    pub fn configured(&self, role: ModelRole) -> Option<&ModelAssignment> {
        self.assignment(role)
            .filter(|assignment| assignment.is_set())
    }

    /// The selection in use. A stored but incomplete assignment blocks fallback.
    pub fn effective(&self, role: ModelRole) -> Option<&ModelAssignment> {
        self.assignment(role)
            .or_else(|| self.assignment(role.fallback()?))
            .filter(|assignment| assignment.is_set())
    }

    fn assignment(&self, role: ModelRole) -> Option<&ModelAssignment> {
        match role {
            ModelRole::Companion => Some(&self.companion),
            ModelRole::Task => Some(&self.task),
            ModelRole::Perception => self.perception.as_ref(),
            ModelRole::Compaction => self.compaction.as_ref(),
            ModelRole::Consolidation => self.consolidation.as_ref(),
            ModelRole::Decision => self.decision.as_ref(),
            ModelRole::Embedding => self.embedding.as_ref(),
        }
    }

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
