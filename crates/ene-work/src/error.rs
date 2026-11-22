//! Error types for ACP bridging and work orchestration.

/// Result alias using [`WorkError`].
pub type Result<T> = std::result::Result<T, WorkError>;

/// Errors returned by ACP subprocess spawning and JSON-RPC bridge transport.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}
