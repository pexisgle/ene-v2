//! REST API v1 wire types. Opaque extension fields retain their existing JSON format.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Companion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Streaming,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Message {
    pub id: String,
    pub companion_id: String,
    pub role: Role,
    pub text: String,
    pub status: MessageStatus,
    pub created_at: String,
    pub sequence: u64,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub in_reply_to: Option<String>,
    pub task_id: Option<String>,
    pub question_id: Option<String>,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Companion {
    pub id: String,
    pub name: String,
    pub body: Option<Value>,
    pub voice: Option<Value>,
    pub mind: String,
    pub model_assignments: ModelAssignments,
    pub availability: Value,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ModelAssignments {
    pub companion: Option<ModelAssignment>,
    pub task: Option<ModelAssignment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Connection>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Snapshot {
    pub host_id: String,
    pub environment_id: String,
    pub capabilities: Value,
    pub cursor: Cursor,
    pub companions: Vec<Companion>,
    pub messages: Vec<Message>,
    pub message_cursors: BTreeMap<String, Option<String>>,
    pub tasks: Vec<Value>,
    pub questions: Vec<Value>,
    pub settings: Settings,
    pub grants: Vec<DataTransferGrant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Notification {
    pub epoch: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub companion_id: Option<String>,
    pub data: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}

pub use crate::sync::{Replica, ResyncReason};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Connection {
    pub id: String,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: Value,
    pub credential_state: String,
    pub capabilities: Option<Value>,
    pub last_check: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataTransferGrant {
    pub id: String,
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
    pub status: String,
    pub revision: u64,
    pub invalidation_reason: Option<String>,
    pub connection_fingerprint: Value,
}

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessageAcceptance {
    pub request_id: String,
    pub message_id: String,
    pub response_message_id: String,
}
/// Mutation inputs shared by the API boundary and application operations.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ConnectionInput {
    pub id: Option<String>,
    pub kind: String,
    pub adapter: String,
    pub endpoint: String,
    pub options: Value,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub revision: u64,
    pub connections: Vec<ConnectionInput>,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CompanionPatch {
    pub revision: u64,
    pub name: Option<String>,
    pub mind: Option<String>,
    #[cfg_attr(feature = "openapi", schema(schema_with = assignment_patch_schema))]
    pub model_assignments: Option<BTreeMap<String, Option<ModelAssignment>>>,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct GrantInput {
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: String,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct GrantPatch {
    pub revision: u64,
    pub destinations: Option<Vec<String>>,
    pub data_kinds: Option<Vec<String>>,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub text: String,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub question_id: Option<String>,
    pub decision: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub companion_id: String,
    pub purpose: String,
    pub model_id: Option<String>,
}

/// Provider-independent model reference. IDs are opaque and must be URL-encoded.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelAssignment {
    pub connection_id: String,
    pub model_id: String,
}
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialInput {
    #[cfg_attr(feature = "openapi", schema(write_only))]
    pub secret: Option<String>,
    pub source: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CredentialResult {
    pub credential_state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ProviderEnabled {
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CheckResult {
    pub request_id: String,
    pub capabilities: Value,
    pub success: bool,
    pub model_id: String,
    pub content_chunks: u64,
    pub unicode_scalars: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApiErrorBody {
    pub error: Failure,
    pub request_id: String,
}
/// Initial WebSocket frame; obtain a snapshot for this live subscription.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EventReady {
    #[serde(rename = "type")]
    pub kind: String,
    pub connection_id: String,
    pub cursor: Cursor,
}

// Nullable map values mean explicit removal; absent keys remain unchanged.
// utoipa cannot yet derive nullable additionalProperties automatically.
#[cfg(feature = "openapi")]
fn assignment_patch_schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
    use utoipa::openapi::{
        Ref,
        schema::{ObjectBuilder, OneOfBuilder, Schema, SchemaType, Type},
    };
    ObjectBuilder::new()
        .schema_type(SchemaType::from_iter([Type::Object, Type::Null]))
        .additional_properties(Some(Schema::OneOf(
            OneOfBuilder::new()
                .item(Ref::from_schema_name("ModelAssignment"))
                .item(ObjectBuilder::new().schema_type(Type::Null))
                .build(),
        )))
        .into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GrantResult {
    pub grant: DataTransferGrant,
    pub questions: Vec<Value>,
}
/// Text delta offsets are Unicode scalar counts, never UTF-8 byte offsets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MessageDelta {
    pub message_id: String,
    pub offset: u64,
    pub delta: String,
}
