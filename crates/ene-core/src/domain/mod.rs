//! Host entities independent of HTTP, persistence and provider implementations.
pub mod rules;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Storage and API share values; application state and typed grant rules stay here.
pub use ene_api_types::host::{
    Companion, Connection, Cursor, Failure, Message, MessageAcceptance, MessageStatus,
    ModelAssignment, ModelAssignments, Page, Role, Settings,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub companion_id: String,
    pub kind: String,
    pub connection_id: String,
    pub destinations: Vec<String>,
    pub data_kinds: Vec<String>,
    pub purpose: GrantPurpose,
    pub status: GrantStatus,
    pub revision: u64,
    pub invalidation_reason: Option<String>,
    pub connection_fingerprint: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Configuration {
    pub companions: std::collections::BTreeMap<String, Companion>,
    pub settings: Settings,
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub credential_refs: std::collections::BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub host_id: String,
    pub environment_id: String,
    pub companions: Vec<Companion>,
    pub settings: Settings,
    pub messages: Vec<Message>,
    pub message_cursors: std::collections::BTreeMap<String, Option<String>>,
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
    ExternalFailure,
    ProviderFailure,
    StorageFailure,
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

pub use crate::ports::Accepted;
pub use Connection as ProviderConnection;
pub use MessageAcceptance as Acceptance;

/// Only non-terminal messages can advance. Applied text remains available on failure/cancel.
pub fn transition(
    message: &mut Message,
    status: MessageStatus,
    delta: Option<String>,
    failure: Option<Failure>,
) -> Result<(), Error> {
    if matches!(message.status, MessageStatus::Unknown(_)) {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    if matches!(
        message.status,
        MessageStatus::Completed | MessageStatus::Failed | MessageStatus::Cancelled
    ) || matches!(
        (&message.status, &status),
        (MessageStatus::Streaming, MessageStatus::Queued)
    ) {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    message.status = status;
    if let Some(text) = delta {
        message.text.push_str(&text);
    }
    message.failure = failure;
    Ok(())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantStatus {
    Active,
    Invalidated,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantPurpose {
    Conversation,
    ConnectionCheck,
}
impl GrantPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::ConnectionCheck => "connection_check",
        }
    }
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "conversation" => Ok(Self::Conversation),
            "connection_check" => Ok(Self::ConnectionCheck),
            _ => Err(Error::new(ErrorKind::UnsupportedCapability)),
        }
    }
}

impl From<Grant> for ene_api_types::DataTransferGrant {
    fn from(grant: Grant) -> Self {
        Self {
            id: grant.id,
            companion_id: grant.companion_id,
            kind: grant.kind,
            connection_id: grant.connection_id,
            destinations: grant.destinations,
            data_kinds: grant.data_kinds,
            purpose: grant.purpose.as_str().into(),
            status: match grant.status {
                GrantStatus::Active => "active",
                GrantStatus::Invalidated => "invalidated",
            }
            .into(),
            revision: grant.revision,
            invalidation_reason: grant.invalidation_reason,
            connection_fingerprint: grant.connection_fingerprint,
        }
    }
}
