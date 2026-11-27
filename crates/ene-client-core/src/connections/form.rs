//! Editable connection options and the supported subset of adapter schemas.

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

/// Endpoint options for Google Agent Platform.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AgentPlatformOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The Google Cloud project, absent in Express mode.
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The deployment region, defaulting to global on the host.
    pub location: Option<String>,
}

impl AgentPlatformOptions {
    /// Builds options from the two form fields, treating blanks as unset.
    pub fn new(project_id: &str, location: &str) -> Self {
        Self {
            project_id: super::nonempty(project_id).map(str::to_owned),
            location: super::nonempty(location).map(str::to_owned),
        }
    }

    /// Serializes the options for the connection endpoint.
    pub fn json(&self) -> String {
        serde_json::json!(self).to_string()
    }
}

/// The editable subset of the JSON Schema supplied by connection adapters.
#[derive(Debug, Deserialize)]
pub struct OptionsSchema {
    /// Fields keyed by their option names.
    pub properties: BTreeMap<String, FieldSchema>,
}

/// One field declared by the adapter.
#[derive(Debug, Deserialize)]
pub struct FieldSchema {
    #[serde(rename = "type", default)]
    /// The shape declared for this field.
    pub kind: SchemaType,
    #[serde(rename = "enum", default)]
    choices: Option<Vec<Value>>,
}

impl FieldSchema {
    /// Returns the string choices when the schema declares an enumeration.
    pub fn choices(&self) -> Option<impl Iterator<Item = &str>> {
        self.choices
            .as_ref()
            .map(|values| values.iter().filter_map(Value::as_str))
    }
}

/// Type declaration in an option field schema.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SchemaType {
    /// A supported scalar name.
    Scalar(FieldKind),
    /// A complex type declaration.
    Other(serde::de::IgnoredAny),
}

impl Default for SchemaType {
    fn default() -> Self {
        Self::Scalar(FieldKind::Text)
    }
}

impl SchemaType {
    /// Selects the editing control, falling back to a text field.
    pub fn scalar(&self) -> FieldKind {
        match self {
            Self::Scalar(kind) => *kind,
            Self::Other(_) => FieldKind::Text,
        }
    }
}

/// Scalar controls supported in the options form.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// A toggle.
    Boolean,
    /// An integer field.
    Integer,
    /// A number field.
    Number,
    /// A text field.
    #[serde(other)]
    Text,
}

impl FieldKind {
    /// Parses a draft using the numeric field kind, keeping invalid input for validation.
    pub fn draft_value(self, text: &str) -> Value {
        let number = match self {
            Self::Integer => text.parse::<i64>().ok().map(Number::from),
            Self::Number => text.parse::<f64>().ok().and_then(Number::from_f64),
            _ => None,
        };
        number.map_or_else(|| Value::String(text.to_owned()), Value::Number)
    }
}

/// Settings keyed by their schema property name.
pub type Options = BTreeMap<String, Value>;

/// Formats the current value for a text control.
pub fn draft_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_known_fields_keeps_adapter_extensions() {
        let schema: OptionsSchema = serde_json::from_str(
            r#"{"properties":{"count":{"type":"integer"},"variant":{"type":["string","null"],"enum":["on",null]}}}"#,
        )
        .unwrap();
        let mut options: Options =
            serde_json::from_str(r#"{"count":1,"adapter_extension":{"values":[true,null]}}"#)
                .unwrap();
        options.insert(
            "count".to_owned(),
            schema.properties["count"].kind.scalar().draft_value("42"),
        );
        assert_eq!(
            schema.properties["variant"]
                .choices()
                .unwrap()
                .collect::<Vec<_>>(),
            ["on"]
        );
        assert_eq!(
            serde_json::json!(options),
            serde_json::json!({"count": 42, "adapter_extension": {"values": [true, null]}})
        );
    }
}
