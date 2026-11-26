//! User-visible failure representation shared across domain models and persistence.

use serde::{Deserialize, Serialize};

/// A user-visible failure: a stable code and a short, non-secret message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// Stable machine-readable code, safe to branch on.
    pub code: String,
    /// Short human-readable message carrying no secret.
    pub message: String,
}

impl Failure {
    /// Creates a new failure value with a machine-readable `code` and a concise `message`.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
