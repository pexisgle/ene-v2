//! JSON documents on the wire and the schema validation the host performs.
//!
//! Capability inputs, capability outputs and `device_request` arguments are
//! validated with the `jsonschema` crate against the schema the client
//! declared, so the standard keywords (`pattern`, ``, `anyOf`, ...) behave
//! as specified instead of a hand-written subset.
use super::error::{HostError, Result};
use hashlink::LruCache;
use serde_json::{Value, json};
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
            return Err(HostError::Invalid(
                ("json document schema is too long").into(),
            ));
        }
        if !self.schema.is_empty() && !self.json.is_object() && !self.json.is_array() {
            return Err(HostError::Invalid(
                ("a schema'd json document must be an object or an array").into(),
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
        .map_err(|_| HostError::Invalid(("arguments must be a JSON object").into()))?;
    if !value.is_object() {
        return Err(HostError::Invalid(
            ("arguments must be a JSON object").into(),
        ));
    }
    Ok(value)
}

/// Parses a JSON Schema document, which must be an object.
pub(crate) fn parse_schema(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({"type": "object"}));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| HostError::Invalid(("input schema must be a JSON object").into()))?;
    if !value.is_object() {
        return Err(HostError::Invalid(
            ("input schema must be a JSON object").into(),
        ));
    }
    Ok(value)
}

fn validator(schema: &Value) -> std::result::Result<Arc<jsonschema::Validator>, String> {
    static CACHE: OnceLock<Mutex<LruCache<String, Arc<jsonschema::Validator>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(LruCache::new(256)));
    let key = serde_json::to_string(schema)
        .map_err(|error| format!("input: schema is not serializable: {error}"))?;
    if let Some(validator) = cache.lock().ok().and_then(|mut map| map.get(&key).cloned()) {
        return Ok(validator);
    }
    let validator = Arc::new(
        jsonschema::validator_for(schema)
            .map_err(|error| format!("input: schema is not valid: {error}"))?,
    );
    if let Ok(mut map) = cache.lock() {
        if let Some(cached) = map.get(&key) {
            return Ok(Arc::clone(cached));
        }
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
