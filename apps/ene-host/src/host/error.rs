//! The host's own failure type. Connect status mapping lives at the RPC edge,
//! so the host stays independent of any transport.
pub(crate) type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum HostError {
    /// The request is malformed or carries an unsupported value.
    #[error("{0}")]
    Invalid(String),
    /// The addressed entity does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request contradicts current state, such as a stale revision.
    #[error("{0}")]
    Conflict(String),
    /// The addressed entity already exists.
    #[error("{0}")]
    AlreadyExists(String),
    /// The caller is not allowed to perform the operation.
    #[error("{0}")]
    Permission(String),
    /// A bounded resource, such as the command queue, is exhausted.
    #[error("{0}")]
    Exhausted(String),
    /// The capability exists but is not available right now.
    #[error("{0}")]
    Unavailable(String),
    /// The host is correctly configured, but this operation needs setup first.
    #[error("{0}")]
    FailedPrecondition(String),
    /// The state database or the session log failed. The host stops.
    #[error("{0}")]
    Storage(String),
    /// A defect the host cannot classify.
    #[error("{0}")]
    Internal(String),
}

impl HostError {
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
            | Self::FailedPrecondition(message)
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
            Self::FailedPrecondition(_) => "failed_precondition",
            Self::Storage(_) => "storage_error",
            Self::Internal(_) => "internal",
        }
    }
}

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
        // A lock another process holds is transient: the host keeps serving
        // and the caller can retry. Every other database failure means the
        // state can no longer be trusted, and the host stops.
        if let rusqlite::Error::SqliteFailure(code, _) = &error
            && matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        {
            return Self::Unavailable(format!("the state database is busy: {error}"));
        }
        Self::Storage(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(code: rusqlite::ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            None,
        )
    }

    #[test]
    fn a_busy_database_is_transient_and_a_broken_one_stops_the_host() {
        for code in [
            rusqlite::ErrorCode::DatabaseBusy,
            rusqlite::ErrorCode::DatabaseLocked,
        ] {
            let mapped = HostError::from(sqlite(code));
            assert!(!mapped.is_storage(), "{code:?} must not stop the host");
            assert!(matches!(mapped, HostError::Unavailable(_)), "{code:?}");
        }
        for code in [
            rusqlite::ErrorCode::DatabaseCorrupt,
            rusqlite::ErrorCode::DiskFull,
            rusqlite::ErrorCode::SystemIoFailure,
        ] {
            let mapped = HostError::from(sqlite(code));
            assert!(mapped.is_storage(), "{code:?} must stop the host");
        }
    }
}
