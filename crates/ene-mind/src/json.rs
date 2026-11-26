//! Schema-tagged JSON documents stored for companion body, voice, and mind state.
use crate::Result;
use ene_domain::DomainError;
use serde_json::{Value, json};

/// A schema-tagged JSON document (`JsonDocument` wire type).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonDoc {
    pub schema: String,
    pub json: Value,
}

impl JsonDoc {
    /// Creates a new schema-tagged JSON document.
    pub fn new(schema: impl Into<String>, json: Value) -> Self {
        Self {
            schema: schema.into(),
            json,
        }
    }

    /// Creates an empty, un-schema'd JSON document.
    pub fn empty() -> Self {
        Self::new("", json!({}))
    }

    /// Checks the schema identifier and that a schema'd document is an object
    /// or an array; plain JSON text is accepted when no schema is named.
    pub fn validate(&self) -> Result<()> {
        if self.schema.chars().count() > 128 {
            return Err(DomainError::Invalid(
                "json document schema is too long".into(),
            ));
        }
        if !self.schema.is_empty() && !self.json.is_object() && !self.json.is_array() {
            return Err(DomainError::Invalid(
                "a schema'd json document must be an object or an array".into(),
            ));
        }
        Ok(())
    }
}
