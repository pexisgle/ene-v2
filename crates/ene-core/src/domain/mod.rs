//! Host entities independent of HTTP, persistence and provider implementations.
use serde::{Deserialize, Serialize};

// These values have the same representation in storage and the public contract.
// Keep one definition; application-only state and errors remain below.
pub use ene_api_types::host::{
    Companion, Connection, Cursor, DataTransferGrant as Grant, Failure, Message, MessageAcceptance,
    MessageStatus, ModelAssignment, ModelAssignments, Page, Role, Settings,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Configuration {
    pub companion: Companion,
    pub settings: Settings,
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub credential_refs: std::collections::BTreeMap<String, String>,
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
