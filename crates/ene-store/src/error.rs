//! Structured storage error type shared by all repositories in `ene-store`.

/// Convenience result alias for `ene-store` operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Errors raised during SQLite persistence, schema migrations, and constraint checks.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The caller supplied an invalid identifier, JSON document, schedule, or value.
    #[error("{0}")]
    Invalid(String),
    /// The addressed entity does not exist.
    #[error("{0}")]
    NotFound(String),
    /// Optimistic revision check or idempotency guard failed.
    #[error("{0}")]
    Conflict(String),
    /// Operation attempted outside the permitted companion scope.
    #[error("{0}")]
    Permission(String),
    /// Database lock contention that callers may retry safely.
    #[error("{0}")]
    Unavailable(String),
    /// Corrupt stored row, unsupported schema version, or fatal SQLite I/O error.
    #[error("{0}")]
    Storage(String),
}

impl StoreError {
    /// Whether this failure indicates corrupt or unreadable state storage.
    pub fn is_storage(&self) -> bool {
        matches!(self, Self::Storage(_))
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
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

impl From<ene_domain::DomainError> for StoreError {
    fn from(error: ene_domain::DomainError) -> Self {
        Self::Invalid(error.to_string())
    }
}
