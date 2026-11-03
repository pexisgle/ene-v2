//! Exhaustive host-owned execution policy. Adapters cannot choose consent scope.
use super::{Error, ErrorKind, GrantPurpose};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Conversation,
    ConnectionCheck,
    ModelList,
    Task,
    Embedding,
}

pub struct OperationSpec {
    pub purpose: GrantPurpose,
    pub data: &'static [&'static str],
    pub timeout: Duration,
    pub output_bytes: usize,
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::ConnectionCheck => "connection_check",
            Self::ModelList => "model_list",
            Self::Task => "task",
            Self::Embedding => "embedding",
        }
    }
    pub const ALL: [Self; 5] = [
        Self::Conversation,
        Self::ConnectionCheck,
        Self::ModelList,
        Self::Task,
        Self::Embedding,
    ];
    pub const fn spec(self) -> OperationSpec {
        match self {
            Self::Conversation => OperationSpec {
                purpose: GrantPurpose::Conversation,
                data: &["conversation"],
                timeout: Duration::from_secs(180),
                output_bytes: 128_000,
            },
            Self::ConnectionCheck => OperationSpec {
                purpose: GrantPurpose::ConnectionCheck,
                data: &["test_input"],
                timeout: Duration::from_secs(90),
                output_bytes: 8192,
            },
            Self::ModelList => OperationSpec {
                purpose: GrantPurpose::ConnectionCheck,
                data: &["metadata"],
                timeout: Duration::from_secs(30),
                output_bytes: 8 * 1024 * 1024,
            },
            Self::Task => OperationSpec {
                purpose: GrantPurpose::Task,
                data: &["task"],
                timeout: Duration::from_secs(300),
                output_bytes: 256_000,
            },
            Self::Embedding => OperationSpec {
                // Embeddings are background index work, sent under the task
                // grant rather than the parent conversation's grant.
                purpose: GrantPurpose::Task,
                data: &["task"],
                timeout: Duration::from_secs(60),
                output_bytes: 8 * 1024 * 1024,
            },
        }
    }
}

pub const MAX_EXECUTIONS: usize = 8;
pub const MAX_PENDING_MUTATIONS: usize = 64;
pub const MAX_INPUT_BYTES: usize = 65_536;
pub const MAX_MODEL_ID_BYTES: usize = 512;
pub const MAX_MODELS: usize = 4096;

pub fn validate_model_id(value: &str) -> Result<(), Error> {
    if value.trim().is_empty()
        || value.len() > MAX_MODEL_ID_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(Error::invalid());
    }
    Ok(())
}

/// Revisions never wrap and never silently return to an earlier value.
pub fn next_revision(value: u64) -> Result<u64, Error> {
    value
        .checked_add(1)
        .ok_or_else(|| Error::new(ErrorKind::StateConflict))
}

/// A host-meaningful id: non-empty, bounded and free of control characters.
pub fn validate_id(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(Error::invalid());
    }
    Ok(())
}
pub fn validate_request_key(value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.len() > 200
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
    {
        return Err(Error::invalid());
    }
    Ok(())
}
