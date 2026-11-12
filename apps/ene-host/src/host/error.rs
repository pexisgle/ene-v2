//! The host's error type: domain failures stay independent of Connect.
use std::fmt;

pub(crate) type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug)]
pub(crate) enum HostError {
    Invalid(String),
    NotFound(String),
    Conflict(String),
    AlreadyExists(String),
    Permission(String),
    Exhausted(String),
    Unavailable(String),
    Storage(String),
    Internal(String),
}

impl HostError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub(crate) fn already_exists(message: impl Into<String>) -> Self {
        Self::AlreadyExists(message.into())
    }

    pub(crate) fn permission(message: impl Into<String>) -> Self {
        Self::Permission(message.into())
    }

    pub(crate) fn exhausted(message: impl Into<String>) -> Self {
        Self::Exhausted(message.into())
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(crate) fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Whether this failure means the state database can no longer be trusted.
    pub(crate) fn is_storage(&self) -> bool {
        matches!(self, Self::Storage(_))
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::AlreadyExists(message)
            | Self::Permission(message)
            | Self::Exhausted(message)
            | Self::Unavailable(message)
            | Self::Storage(message)
            | Self::Internal(message) => message,
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HostError {}

impl From<rusqlite::Error> for HostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<serde_json::Error> for HostError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<std::io::Error> for HostError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}
