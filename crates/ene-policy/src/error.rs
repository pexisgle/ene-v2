//! Structured errors for capability declaration and policy validation.
use thiserror::Error;

/// Errors returned by capability declaration checks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyError {
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, PolicyError>;
