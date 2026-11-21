//! Domain validation errors for companion profiles, mind documents, and memory operations.

/// Error returned by pure `ene-mind` domain validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MindError {
    /// A companion field or JSON document violated domain constraints.
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, MindError>;
