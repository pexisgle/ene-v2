//! JSON documents on the wire and the schema validation the host performs.
//!
//! Capability inputs, capability outputs and `device_request` arguments are
//! validated with the `jsonschema` crate against the schema the client
//! declared, so the standard keywords (`pattern`, ``, `anyOf`, ...) behave
//! as specified instead of a hand-written subset.
use super::error::{HostError, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// A schema-tagged JSON document, the `JsonDocument` wire type.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct JsonDoc {
    pub(crate) schema: String,
    pub(crate) json: Value,
}

impl JsonDoc {
    pub(crate) fn new(schema: impl Into<String>, json: Value) -> Self {
        Self {
            schema: schema.into(),
            json,
        }
    }

    pub(crate) fn empty() -> Self {
        Self::new("", json!({}))
    }

    /// Checks the schema identifier and that a schema'd document is an object
    /// or an array; plain JSON text is accepted when no schema is named.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema.chars().count() > 128 {
            return Err(HostError::invalid("json document schema is too long"));
        }
        if !self.schema.is_empty() && !self.json.is_object() && !self.json.is_array() {
            return Err(HostError::invalid(
                "a schema'd json document must be an object or an array",
            ));
        }
        Ok(())
    }
}

/// Parses an optional capability input or output, which the contract types as
/// JSON object text.
pub(crate) fn parse_object(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| HostError::invalid("arguments must be a JSON object"))?;
    if !value.is_object() {
        return Err(HostError::invalid("arguments must be a JSON object"));
    }
    Ok(value)
}

/// Parses a JSON Schema document, which must be an object.
pub(crate) fn parse_schema(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({"type": "object"}));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| HostError::invalid("input schema must be a JSON object"))?;
    if !value.is_object() {
        return Err(HostError::invalid("input schema must be a JSON object"));
    }
    Ok(value)
}

/// Compiles `schema` once and reuses the validator. Client schemas are
/// stable between declarations, so validation does not recompile them on
/// every capability call.
fn validator(schema: &Value) -> std::result::Result<Arc<jsonschema::Validator>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<jsonschema::Validator>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = serde_json::to_string(schema)
        .map_err(|error| format!("input: schema is not serializable: {error}"))?;
    if let Some(validator) = cache.lock().ok().and_then(|map| map.get(&key).cloned()) {
        return Ok(validator);
    }
    let validator = Arc::new(
        jsonschema::validator_for(schema)
            .map_err(|error| format!("input: schema is not valid: {error}"))?,
    );
    if let Ok(mut map) = cache.lock() {
        map.insert(key, Arc::clone(&validator));
    }
    Ok(validator)
}

/// Validates `instance` against `schema` with the standard JSON Schema
/// validator. An error is a client-visible explanation without a path dump.
pub(crate) fn validate(schema: &Value, instance: &Value) -> std::result::Result<(), String> {
    validator(schema)?
        .validate(instance)
        .map_err(|error| format!("input: {error}"))
}
