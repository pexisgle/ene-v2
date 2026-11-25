//! Structured errors for capability declaration and policy validation.
use thiserror::Error;

/// Errors returned by capability declaration checks.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyError {
    /// The declaration breaks a stated rule; the payload says which.
    #[error("{0}")]
    Invalid(String),
}

/// Result alias for policy validation.
pub type Result<T> = std::result::Result<T, PolicyError>;
