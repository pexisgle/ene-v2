use serde::{Deserialize, Serialize};

/// A host-facing failure: the plugin's own error code plus a readable message.
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct Failure {
    /// Stable machine-readable code the host branches on.
    pub code: String,
    /// Short human-readable message carrying no secret.
    pub message: String,
}
impl Failure {
    /// Creates a failure whose message is the code, for a code that speaks for itself.
    pub fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            message: code.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered form is what the host logs and shows, so the derive must
    /// reproduce the previous hand-written `"{code}: {message}"` exactly.
    #[test]
    fn a_failure_renders_as_its_code_then_its_message() {
        let explicit = Failure {
            code: "plugin_denied".into(),
            message: "the capability was not granted".into(),
        };
        assert_eq!(
            explicit.to_string(),
            "plugin_denied: the capability was not granted"
        );

        // `new` sets both fields from the code, and that must still render.
        let shorthand = Failure::new("plugin_unavailable");
        assert_eq!(
            shorthand.to_string(),
            "plugin_unavailable: plugin_unavailable"
        );

        // It must still be usable as a boxed error.
        let boxed: Box<dyn std::error::Error> = Box::new(explicit);
        assert!(boxed.to_string().starts_with("plugin_denied: "));
    }
}
