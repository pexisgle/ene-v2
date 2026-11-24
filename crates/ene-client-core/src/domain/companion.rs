//! Pure companion, mind, chat message, turn, and binding domain types.

use super::{Failure, JsonDocument};
use ene_domain::CompanionId;

/// Binding from a companion role slot to a connection and model identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelAssignment {
    pub connection_id: String,
    pub model_id: String,
}

/// Per-role model assignments for a companion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelAssignments {
    pub companion: Option<ModelAssignment>,
    pub task: Option<ModelAssignment>,
    pub perception: Option<ModelAssignment>,
    pub compaction: Option<ModelAssignment>,
    pub consolidation: Option<ModelAssignment>,
    pub decision: Option<ModelAssignment>,
    pub embedding: Option<ModelAssignment>,
}

/// Structured payload for `Companion.body` (`schema = "ene.body.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BodyBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_percent: Option<f32>,
}

/// Structured payload for `Companion.voice` (`schema = "ene.voice.v1"`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoiceBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// Structured personality profile stored on a companion's mind.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindProfile {
    #[serde(default)]
    pub core: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub facets: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    #[serde(default)]
    pub background: Vec<String>,
    #[serde(default)]
    pub expression: Vec<String>,
}

/// Dynamic mind state snapshots maintained by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindState {
    pub revision: u64,
    pub emotion: Option<JsonDocument>,
    pub relationships: Option<JsonDocument>,
    pub tendencies: Option<JsonDocument>,
    pub concerns: Option<JsonDocument>,
    pub updated_at: String,
}

/// Memory indexing statistics for a companion's mind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindIndexStatus {
    pub pending_fts_count: u32,
    pub pending_vector_count: u32,
    pub embedding_model: String,
    pub model_calls: u64,
    pub total_tokens: u64,
}

/// Combined view of a companion's mind profile, dynamic state, and index status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindView {
    pub companion_id: String,
    pub profile: Option<MindProfile>,
    pub state: Option<MindState>,
    pub index: Option<MindIndexStatus>,
}

/// Saved companion configuration and metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Companion {
    pub id: String,
    pub name: String,
    pub mind: String,
    pub body: Option<JsonDocument>,
    pub voice: Option<JsonDocument>,
    pub assignments: Option<ModelAssignments>,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub persona: Option<String>,
    pub voice_identifier: Option<String>,
}

/// Portable starter or exported companion template.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionTemplate {
    // The wire contract emits `formatVersion` and accepts the snake_case
    // spelling older exports used, so both must keep parsing.
    #[serde(default, alias = "format_version")]
    pub format_version: u32,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<MindProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<JsonDocument>,
    #[serde(
        default,
        alias = "voice_identifier",
        skip_serializing_if = "Option::is_none"
    )]
    pub voice_identifier: Option<String>,
}

/// Author role of a recorded chat message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MessageRole {
    #[default]
    Unspecified,
    User,
    Assistant,
    Agent,
    Context,
}

/// Single chat history message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub companion_id: Option<CompanionId>,
    pub work_id: String,
    pub role: MessageRole,
    pub text: String,
    pub created_at: String,
    pub sequence: u64,
    pub request_id: Option<String>,
    pub author: String,
}

/// Execution status of a companion turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnStatus {
    #[default]
    Unspecified,
    Streaming,
    Answered,
    Stopped,
    Failed,
}

/// Turn status notification payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnEvent {
    pub companion_id: Option<CompanionId>,
    pub turn_id: String,
    pub status: TurnStatus,
    pub failure: Option<Failure>,
}

/// Profile fields edited in the companion editor form.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompanionProfilePatch {
    pub companion_id: String,
    pub name: String,
    pub persona: String,
    pub mind: String,
    pub body: BodyBinding,
    pub voice: VoiceBinding,
}

/// Inputs collected when creating a new companion from the studio.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewCompanionSpec {
    pub name: String,
    pub persona: String,
    pub mind: String,
    pub voice_identifier: Option<String>,
}

/// Automatic default model patch computed when a companion lacks required slots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoAssignModelPatch {
    pub companion_id: String,
    pub revision: u64,
    pub assign_companion: bool,
    pub assign_task: bool,
    pub assignment: ModelAssignment,
}
