//! Pure companion, mind, chat message, turn, and binding domain types.

use super::{BodyBinding, Failure, JsonDocument, ModelAssignments, VoiceBinding};
use crate::CompanionId;

/// Structured personality profile stored on a companion's mind.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MindProfile {
    /// The stable core of who the companion is.
    #[serde(default)]
    pub core: String,
    /// Optimistic-concurrency counter, bumped on every write.
    #[serde(default)]
    pub revision: u64,
    /// Named traits the companion expresses.
    #[serde(default)]
    pub facets: Vec<String>,
    /// Worked examples of the companion's voice.
    #[serde(default)]
    pub examples: Vec<String>,
    /// History that shaped the companion.
    #[serde(default)]
    pub background: Vec<String>,
    /// How the companion tends to express itself.
    #[serde(default)]
    pub expression: Vec<String>,
}

/// Dynamic mind state snapshots maintained by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindState {
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Current emotional state.
    pub emotion: Option<JsonDocument>,
    /// Who the companion knows and how it regards them.
    pub relationships: Option<JsonDocument>,
    /// Habits the companion has developed.
    pub tendencies: Option<JsonDocument>,
    /// What the companion is currently occupied with.
    pub concerns: Option<JsonDocument>,
    /// When the state last changed, as the host stamped it.
    pub updated_at: String,
}

/// Memory indexing statistics for a companion's mind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindIndexStatus {
    /// Memories waiting to be indexed for full-text search.
    pub pending_fts_count: u32,
    /// Memories waiting to be embedded.
    pub pending_vector_count: u32,
    /// The model that produces the memory embeddings.
    pub embedding_model: String,
    /// Model calls spent on indexing so far.
    pub model_calls: u64,
    /// Tokens spent on indexing so far.
    pub total_tokens: u64,
}

/// Combined view of a companion's mind profile, dynamic state, and index status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MindView {
    /// The companion this view describes.
    pub companion_id: String,
    /// The saved personality profile, when the host returned one.
    pub profile: Option<MindProfile>,
    /// The dynamic state, when the host returned one.
    pub state: Option<MindState>,
    /// Memory indexing statistics, when the host returned them.
    pub index: Option<MindIndexStatus>,
}

/// Saved companion configuration and metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Companion {
    /// Id of this companion.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// The companion's standing instructions.
    pub mind: String,
    /// Avatar binding, when one is configured.
    pub body: Option<JsonDocument>,
    /// Voice binding, when one is configured.
    pub voice: Option<JsonDocument>,
    /// Per-role model assignments, when the operator set them.
    pub assignments: Option<ModelAssignments>,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Creation time, as the host stamped it.
    pub created_at: String,
    /// Last update, as the host stamped it.
    pub updated_at: String,
    /// Free-text persona shown in the editor.
    pub persona: Option<String>,
    /// Voice identifier, kept beside the voice binding for older clients.
    pub voice_identifier: Option<String>,
}

/// Portable starter or exported companion template.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionTemplate {
    /// Template format version, so an older or newer export can be refused.
    // The wire contract emits `formatVersion` and accepts the snake_case
    // spelling older exports used, so both must keep parsing.
    #[serde(default, alias = "format_version")]
    pub format_version: u32,
    /// Id the template suggests for the new companion.
    #[serde(default)]
    pub id: String,
    /// Display name the template suggests.
    #[serde(default)]
    pub name: String,
    /// The companion's standing instructions.
    #[serde(default)]
    pub mind: String,
    /// Free-text persona.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Personality profile to restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<MindProfile>,
    /// Avatar binding to restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonDocument>,
    /// Voice binding to restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<JsonDocument>,
    /// Voice identifier kept beside the voice binding for older exports.
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
    /// The host did not report a role.
    #[default]
    Unspecified,
    /// The person talking to the companion.
    User,
    /// The companion itself.
    Assistant,
    /// A work agent acting on the operator's behalf.
    Agent,
    /// Material the host injected rather than anyone saying it.
    Context,
}

/// Single chat history message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// Id of this message.
    pub id: String,
    /// The companion the message belongs to, when it belongs to one.
    pub companion_id: Option<CompanionId>,
    /// The work session the message belongs to; empty outside work.
    pub work_id: String,
    /// Who wrote the message.
    pub role: MessageRole,
    /// The message body.
    pub text: String,
    /// Creation time, as the host stamped it.
    pub created_at: String,
    /// Position of the message in the companion's history.
    pub sequence: u64,
    /// The request the message answers, when it answers one.
    pub request_id: Option<String>,
    /// Who wrote it, when the role alone is not specific enough.
    pub author: String,
}

/// Execution status of a companion turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnStatus {
    /// The host did not report a status.
    #[default]
    Unspecified,
    /// The reply is still arriving.
    Streaming,
    /// The reply completed.
    Answered,
    /// A person or the host stopped the turn.
    Stopped,
    /// The turn ended in an error; see `TurnEvent::failure`.
    Failed,
}

/// Turn status notification payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnEvent {
    /// The companion the turn belongs to, when it belongs to one.
    pub companion_id: Option<CompanionId>,
    /// Id of the turn this event reports on.
    pub turn_id: String,
    /// Where the turn is in its lifecycle.
    pub status: TurnStatus,
    /// Why the turn failed, when it did.
    pub failure: Option<Failure>,
}

/// Profile fields edited in the companion editor form.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompanionProfilePatch {
    /// The companion being edited.
    pub companion_id: String,
    /// The profile revision shown by the editor.
    pub revision: u64,
    /// New display name.
    pub name: String,
    /// New free-text persona.
    pub persona: String,
    /// New standing instructions.
    pub mind: String,
    /// New avatar binding, when the model control changed.
    pub body: Option<BodyBinding>,
    /// New voice binding, when the voice controls changed.
    pub voice: Option<VoiceBinding>,
}

/// Inputs collected when creating a new companion from the studio.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewCompanionSpec {
    /// Display name for the new companion.
    pub name: String,
    /// Free-text persona.
    pub persona: String,
    /// Standing instructions.
    pub mind: String,
    /// Voice identifier the operator picked.
    pub voice_identifier: Option<String>,
}
