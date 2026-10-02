//! Host entities independent of HTTP, persistence and provider implementations.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Companion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Streaming,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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
pub struct ModelAssignments {
    pub companion: Option<ModelAssignment>,
    pub task: Option<ModelAssignment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Connection>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelAssignment {
    pub connection_id: String,
    pub model_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Configuration {
    pub companion: Companion,
    pub settings: Settings,
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub credential_refs: std::collections::BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageAcceptance {
    pub request_id: String,
    pub message_id: String,
    pub response_message_id: String,
}
#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub host_id: String,
    pub environment_id: String,
    pub companion: Companion,
    pub settings: Settings,
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, thiserror::Error)]
#[error("{kind:?}")]
pub struct Error {
    pub kind: ErrorKind,
    pub reason: Option<&'static str>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    NotFound,
    RevisionConflict,
    StateConflict,
    IdempotencyConflict,
    RequestInProgress,
    PermissionRequired,
    CredentialMissing,
    UnsupportedCapability,
    Unavailable,
    ResyncRequired,
}
impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Self { kind, reason: None }
    }
    pub fn invalid() -> Self {
        Self::new(ErrorKind::InvalidRequest)
    }
    pub fn unavailable() -> Self {
        Self::new(ErrorKind::Unavailable)
    }
}
