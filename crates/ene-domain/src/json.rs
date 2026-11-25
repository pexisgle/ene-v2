//! JSON Schema parsing, compiled validator caching, and instance validation.

use crate::error::DomainError;
use hashlink::LruCache;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

/// Parses an optional JSON object document, defaulting empty text to `{}`.
pub fn parse_object(text: &str) -> Result<Value, DomainError> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| DomainError::Invalid("arguments must be a JSON object".into()))?;
    if !value.is_object() {
        return Err(DomainError::Invalid(
            "arguments must be a JSON object".into(),
        ));
    }
    Ok(value)
}

/// Parses a JSON Schema object document, defaulting empty text to `{"type":"object"}`.
pub fn parse_schema(text: &str) -> Result<Value, DomainError> {
    if text.trim().is_empty() {
        return Ok(json!({"type": "object"}));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| DomainError::Invalid("input schema must be a JSON object".into()))?;
    if !value.is_object() {
        return Err(DomainError::Invalid(
            "input schema must be a JSON object".into(),
        ));
    }
    Ok(value)
}

/// Error returned while compiling a JSON Schema or validating an instance against it.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The schema could not be serialised, so it cannot be cached or compiled.
    #[error("input: schema is not serializable: {0}")]
    SchemaSerialize(#[from] serde_json::Error),
    /// The schema is valid JSON but not a usable JSON Schema.
    #[error("input: schema is not valid: {0}")]
    InvalidSchema(String),
    /// The instance does not satisfy the schema.
    #[error("input: {0}")]
    Validation(String),
}

fn validator(schema: &Value) -> std::result::Result<Arc<jsonschema::Validator>, SchemaError> {
    static CACHE: LazyLock<Mutex<LruCache<String, Arc<jsonschema::Validator>>>> =
        LazyLock::new(|| Mutex::new(LruCache::new(256)));
    let cache = &*CACHE;
    let key = serde_json::to_string(schema)?;
    if let Some(validator) = cache.lock().get(&key).cloned() {
        return Ok(validator);
    }
    let validator = Arc::new(
        jsonschema::validator_for(schema)
            .map_err(|error| SchemaError::InvalidSchema(error.to_string()))?,
    );
    let mut map = cache.lock();
    if let Some(cached) = map.get(&key) {
        return Ok(Arc::clone(cached));
    }
    map.insert(key, Arc::clone(&validator));
    Ok(validator)
}

/// Validates `instance` against `schema` using a cached JSON Schema validator.
pub fn validate(schema: &Value, instance: &Value) -> std::result::Result<(), SchemaError> {
    validator(schema)?
        .validate(instance)
        .map_err(|error| SchemaError::Validation(error.to_string()))
}
