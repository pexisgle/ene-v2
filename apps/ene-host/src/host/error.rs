//! The host's own failure type. Connect status mapping lives at the RPC edge,
//! so the host stays independent of any transport.
use std::fmt;

pub(crate) type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug)]
pub(crate) enum HostError {
    /// The request is malformed or carries an unsupported value.
    Invalid(String),
    /// The addressed entity does not exist.
    NotFound(String),
    /// The request contradicts current state, such as a stale revision.
    Conflict(String),
    /// The addressed entity already exists.
    AlreadyExists(String),
    /// The caller is not allowed to perform the operation.
    Permission(String),
    /// A bounded resource, such as the command queue, is exhausted.
    Exhausted(String),
    /// The capability exists but is not available right now.
    Unavailable(String),
    /// The state database or the session log failed. The host stops.
    Storage(String),
    /// A defect the host cannot classify.
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

    /// Whether this failure means the host can no longer trust its state and
    /// must stop accepting new work.
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

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_argument",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "aborted",
            Self::AlreadyExists(_) => "already_exists",
            Self::Permission(_) => "permission_denied",
            Self::Exhausted(_) => "resource_exhausted",
            Self::Unavailable(_) => "unavailable",
            Self::Storage(_) => "storage_error",
            Self::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HostError {}

/// A user-visible failure: a stable code and a short, non-secret message.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Failure {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

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
