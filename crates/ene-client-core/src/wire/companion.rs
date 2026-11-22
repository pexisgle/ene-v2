//! Wire conversions for companion, mind, chat message, and turn types.

use crate::domain;
use ene_rpc as w;

impl From<w::ModelAssignment> for domain::ModelAssignment {
    fn from(value: w::ModelAssignment) -> Self {
        Self {
            connection_id: value.connection_id,
            model_id: value.model_id,
        }
    }
}

impl From<domain::ModelAssignment> for w::ModelAssignment {
    fn from(value: domain::ModelAssignment) -> Self {
        Self {
            connection_id: value.connection_id,
            model_id: value.model_id,
            ..Default::default()
        }
    }
}

impl From<w::ModelAssignments> for domain::ModelAssignments {
    fn from(value: w::ModelAssignments) -> Self {
        Self {
            companion: value.companion.into_option().map(Into::into),
            task: value.task.into_option().map(Into::into),
            perception: value.perception.into_option().map(Into::into),
            compaction: value.compaction.into_option().map(Into::into),
            consolidation: value.consolidation.into_option().map(Into::into),
            decision: value.decision.into_option().map(Into::into),
            embedding: value.embedding.into_option().map(Into::into),
        }
    }
}

impl From<domain::ModelAssignments> for w::ModelAssignments {
    fn from(value: domain::ModelAssignments) -> Self {
        Self {
            companion: value.companion.map(Into::into).into(),
            task: value.task.map(Into::into).into(),
            perception: value.perception.map(Into::into).into(),
            compaction: value.compaction.map(Into::into).into(),
            consolidation: value.consolidation.map(Into::into).into(),
            decision: value.decision.map(Into::into).into(),
            embedding: value.embedding.map(Into::into).into(),
            ..Default::default()
        }
    }
}

impl From<w::BodyBinding> for domain::BodyBinding {
    fn from(value: w::BodyBinding) -> Self {
        Self {
            model: value.model,
            monitor: value.monitor,
            position: value.position,
            size_percent: value.size_percent,
        }
    }
}

impl From<domain::BodyBinding> for w::BodyBinding {
    fn from(value: domain::BodyBinding) -> Self {
        Self {
            model: value.model,
            monitor: value.monitor,
            position: value.position,
            size_percent: value.size_percent,
        }
    }
}

impl From<w::VoiceBinding> for domain::VoiceBinding {
    fn from(value: w::VoiceBinding) -> Self {
        Self {
            connection_id: value.connection_id,
            voice_id: value.voice_id,
            speed: value.speed,
            pitch: value.pitch,
            style: value.style,
        }
    }
}

impl From<domain::VoiceBinding> for w::VoiceBinding {
    fn from(value: domain::VoiceBinding) -> Self {
        Self {
            connection_id: value.connection_id,
            voice_id: value.voice_id,
            speed: value.speed,
            pitch: value.pitch,
            style: value.style,
        }
    }
}

impl From<w::MindProfile> for domain::MindProfile {
    fn from(value: w::MindProfile) -> Self {
        Self {
            core: value.core,
            revision: value.revision,
            facets: value.facets,
            examples: value.examples,
            background: value.background,
            expression: value.expression,
        }
    }
}

impl From<domain::MindProfile> for w::MindProfile {
    fn from(value: domain::MindProfile) -> Self {
        Self {
            core: value.core,
            revision: value.revision,
            facets: value.facets,
            examples: value.examples,
            background: value.background,
            expression: value.expression,
            ..Default::default()
        }
    }
}

impl From<w::MindState> for domain::MindState {
    fn from(value: w::MindState) -> Self {
        Self {
            revision: value.revision,
            emotion: value.emotion.into_option().map(Into::into),
            relationships: value.relationships.into_option().map(Into::into),
            tendencies: value.tendencies.into_option().map(Into::into),
            concerns: value.concerns.into_option().map(Into::into),
            updated_at: value.updated_at,
        }
    }
}

impl From<w::MindIndexStatus> for domain::MindIndexStatus {
    fn from(value: w::MindIndexStatus) -> Self {
        Self {
            pending_fts_count: value.pending_fts_count,
            pending_vector_count: value.pending_vector_count,
            embedding_model: value.embedding_model,
            model_calls: value.model_calls,
            total_tokens: value.total_tokens,
        }
    }
}

impl From<w::MindView> for domain::MindView {
    fn from(value: w::MindView) -> Self {
        Self {
            companion_id: value.companion_id,
            profile: value.profile.into_option().map(Into::into),
            state: value.state.into_option().map(Into::into),
            index: value.index.into_option().map(Into::into),
        }
    }
}

impl From<w::Companion> for domain::Companion {
    fn from(value: w::Companion) -> Self {
        Self {
            id: value.id,
            name: value.name,
            mind: value.mind,
            body: value.body.into_option().map(Into::into),
            voice: value.voice.into_option().map(Into::into),
            assignments: value.assignments.into_option().map(Into::into),
            revision: value.revision,
            created_at: value.created_at,
            updated_at: value.updated_at,
            persona: value.persona,
            voice_identifier: value.voice_identifier,
        }
    }
}

impl From<w::CompanionTemplate> for domain::CompanionTemplate {
    fn from(value: w::CompanionTemplate) -> Self {
        Self {
            format_version: value.format_version,
            id: value.id,
            name: value.name,
            mind: value.mind,
            persona: value.persona,
            profile: value.profile.into_option().map(Into::into),
            body: value.body.into_option().map(Into::into),
            voice: value.voice.into_option().map(Into::into),
            voice_identifier: value.voice_identifier,
        }
    }
}

impl From<domain::CompanionTemplate> for w::CompanionTemplate {
    fn from(value: domain::CompanionTemplate) -> Self {
        Self {
            format_version: value.format_version,
            id: value.id,
            name: value.name,
            mind: value.mind,
            persona: value.persona,
            profile: value.profile.map(Into::into).into(),
            body: value.body.map(Into::into).into(),
            voice: value.voice.map(Into::into).into(),
            voice_identifier: value.voice_identifier,
            ..Default::default()
        }
    }
}

impl From<w::MessageRole> for domain::MessageRole {
    fn from(value: w::MessageRole) -> Self {
        match value {
            w::MessageRole::Unspecified => Self::Unspecified,
            w::MessageRole::User => Self::User,
            w::MessageRole::Assistant => Self::Assistant,
            w::MessageRole::Agent => Self::Agent,
            w::MessageRole::Context => Self::Context,
        }
    }
}

impl From<w::Message> for domain::Message {
    fn from(value: w::Message) -> Self {
        Self {
            id: value.id,
            companion_id: value.companion_id,
            work_id: value.work_id,
            role: value.role.as_known().map(Into::into).unwrap_or_default(),
            text: value.text,
            created_at: value.created_at,
            sequence: value.sequence,
            request_id: value.request_id,
            author: value.author,
        }
    }
}

impl From<w::TurnStatus> for domain::TurnStatus {
    fn from(value: w::TurnStatus) -> Self {
        match value {
            w::TurnStatus::Unspecified => Self::Unspecified,
            w::TurnStatus::Streaming => Self::Streaming,
            w::TurnStatus::Answered => Self::Answered,
            w::TurnStatus::Stopped => Self::Stopped,
            w::TurnStatus::Failed => Self::Failed,
        }
    }
}

impl From<w::TurnEvent> for domain::TurnEvent {
    fn from(value: w::TurnEvent) -> Self {
        Self {
            companion_id: value.companion_id,
            turn_id: value.turn_id,
            status: value.status.as_known().map(Into::into).unwrap_or_default(),
            failure: value.failure.into_option().map(Into::into),
        }
    }
}

/// Builds a wire `AssignmentPatch` setting a role slot.
pub fn assignment_patch_set(assignment: domain::ModelAssignment) -> w::AssignmentPatch {
    w::AssignmentPatch {
        change: Some(w::assignment_patch::Change::Set(Box::new(
            assignment.into(),
        ))),
        ..Default::default()
    }
}

/// Builds a wire `AssignmentPatch` clearing a role slot.
pub fn assignment_patch_clear() -> w::AssignmentPatch {
    w::AssignmentPatch {
        change: Some(w::assignment_patch::Change::Clear(Box::default())),
        ..Default::default()
    }
}

/// Builds a wire `AssignmentPatch` from an optional domain assignment.
pub fn assignment_patch_from(assignment: Option<domain::ModelAssignment>) -> w::AssignmentPatch {
    match assignment {
        Some(assignment) => assignment_patch_set(assignment),
        None => assignment_patch_clear(),
    }
}
