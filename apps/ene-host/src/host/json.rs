//! JSON documents on the wire and the subset of JSON Schema the host checks.
//!
//! Capability inputs, capability outputs and `device_request` arguments are
//! validated against the schema the client declared. The host deliberately
//! implements a small, explicit subset of JSON Schema (`type`, `properties`,
//! `required`, `additionalProperties`, `items`, `enum`, `minimum`, `maximum`,
//! `minLength`, `maxLength`, `anyOf`, `oneOf`): enough to reject malformed or
//! out-of-contract payloads before they leave the host, without pulling a full
//! validator into a descriptor-driven local API.
use super::error::{HostError, Result};
use serde_json::{Value, json};

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

/// Validates `instance` against the supported JSON Schema subset. An error is
/// a client-visible explanation without a path dump.
pub(crate) fn validate(schema: &Value, instance: &Value) -> std::result::Result<(), String> {
    validate_at(schema, instance, "input")
}

fn validate_at(schema: &Value, instance: &Value, path: &str) -> std::result::Result<(), String> {
    let Some(object) = schema.as_object() else {
        // A boolean schema is not part of the contract: refuse it instead of
        // silently accepting everything.
        return Err(format!("{path}: schema is not an object"));
    };
    if let Some(expected) = object.get("type") {
        let types: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => return Err(format!("{path}: schema type is not a string")),
        };
        if !types.iter().any(|name| matches_type(name, instance)) {
            return Err(format!("{path}: expected type {}", types.join(" or ")));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values.iter().any(|value| value == instance)
    {
        return Err(format!("{path}: value is not one of the allowed values"));
    }
    if let Some(items) = object.get("items")
        && let Value::Array(array) = instance
    {
        for (index, item) in array.iter().enumerate() {
            validate_at(items, item, &format!("{path}[{index}]"))?;
        }
    }
    if let Some(Value::Object(instance)) = Some(instance) {
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !instance.contains_key(name) {
                    return Err(format!("{path}.{name}: required field is missing"));
                }
            }
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (name, value) in instance {
                if let Some(subschema) = properties.get(name) {
                    validate_at(subschema, value, &format!("{path}.{name}"))?;
                } else if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Err(format!("{path}.{name}: unexpected field"));
                }
            }
        }
    }
    if let Some(Value::String(text)) = Some(instance) {
        if let Some(min) = object.get("minLength").and_then(Value::as_u64)
            && (text.chars().count() as u64) < min
        {
            return Err(format!("{path}: string is shorter than {min}"));
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64)
            && (text.chars().count() as u64) > max
        {
            return Err(format!("{path}: string is longer than {max}"));
        }
    }
    if let Value::Number(number) = instance {
        if let Some(min) = object.get("minimum").and_then(Value::as_f64)
            && number.as_f64().is_some_and(|value| value < min)
        {
            return Err(format!("{path}: number is below {min}"));
        }
        if let Some(max) = object.get("maximum").and_then(Value::as_f64)
            && number.as_f64().is_some_and(|value| value > max)
        {
            return Err(format!("{path}: number is above {max}"));
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = object.get(keyword).and_then(Value::as_array) {
            let matched = branches
                .iter()
                .filter(|branch| validate_at(branch, instance, path).is_ok())
                .count();
            let ok = if keyword == "anyOf" {
                matched >= 1
            } else {
                matched == 1
            };
            if !ok {
                return Err(format!("{path}: value does not match {keyword}"));
            }
        }
    }
    Ok(())
}

fn matches_type(name: &str, instance: &Value) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_subset_rejects_what_it_must() {
        let schema = json!({
            "type": "object",
            "required": ["window", "edge"],
            "additionalProperties": false,
            "properties": {
                "window": {"type": "string", "minLength": 1},
                "edge": {"type": "string", "enum": ["top", "bottom"]},
                "enabled": {"type": "boolean"}
            }
        });
        validate(&schema, &json!({"window": "a.rs", "edge": "top"})).unwrap();
        for bad in [
            json!({"edge": "top"}),
            json!({"window": "", "edge": "top"}),
            json!({"window": "a.rs", "edge": "left"}),
            json!({"window": "a.rs", "edge": "top", "extra": 1}),
            json!([1, 2]),
        ] {
            assert!(validate(&schema, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn arguments_must_be_objects() {
        assert!(parse_object("[]").is_err());
        assert_eq!(parse_object("").unwrap(), json!({}));
    }
}
