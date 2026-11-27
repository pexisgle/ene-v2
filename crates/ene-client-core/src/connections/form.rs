//! Lossless edits to saved provider options.

use crate::domain::ConnectionEndpoint;
use ene_domain::{DomainError, json::parse_schema};
use serde_json::{Map, Value};
use std::{collections::HashSet, fmt};

type Options = Map<String, Value>;

fn options(text: &str) -> Result<Options, DomainError> {
    if text.trim().is_empty() {
        return Ok(Options::new());
    }
    serde_json::from_str(text).map_err(|_| {
        DomainError::Invalid("オプションを JSON オブジェクトで入力してください。".into())
    })
}

/// Options from declared editable fields, with all other saved values retained.
pub struct SchemaOptions {
    saved: Options,
    allowed: HashSet<String>,
}

impl fmt::Debug for SchemaOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaOptions").finish_non_exhaustive()
    }
}

impl SchemaOptions {
    /// Loads the fields that an operator can inspect and edit.
    pub fn from_endpoint(endpoint: &ConnectionEndpoint) -> Result<Option<Self>, DomainError> {
        let schema = parse_schema(&endpoint.options_schema)?;
        let Some(fields) = schema.get("properties").and_then(Value::as_object) else {
            return Ok(None);
        };
        let allowed: HashSet<_> = fields
            .iter()
            .filter(|(_, field)| {
                field.get("writeOnly").and_then(Value::as_bool) != Some(true)
                    && field.get("readOnly").and_then(Value::as_bool) != Some(true)
                    && field.get("format").and_then(Value::as_str) != Some("password")
            })
            .map(|(key, _)| key.clone())
            .collect();
        if allowed.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            saved: options(&endpoint.options)?,
            allowed,
        }))
    }

    /// The inspectable values suitable for initializing a JSON input.
    pub fn visible(&self) -> Options {
        self.saved
            .iter()
            .filter(|(key, _)| self.allowed.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Applies edited fields and preserves the stored bytes on an unchanged draft.
    pub fn apply(mut self, visible: &Options, draft: &str) -> Result<Option<String>, DomainError> {
        let edited = options(draft)?;
        if &edited == visible {
            return Ok(None);
        }
        if edited.keys().any(|key| !self.allowed.contains(key)) {
            return Err(DomainError::Invalid(
                "編集できないオプションが含まれています。".into(),
            ));
        }
        self.saved.retain(|key, _| !self.allowed.contains(key));
        self.saved.extend(edited);
        Ok(Some(Value::Object(self.saved).to_string()))
    }
}

/// Google Agent Platform keeps its dedicated project and region fields.
pub struct AgentPlatformOptions(Options);

impl fmt::Debug for AgentPlatformOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentPlatformOptions")
            .finish_non_exhaustive()
    }
}

impl AgentPlatformOptions {
    /// Loads the saved fields without discarding extra settings.
    pub fn load(saved: &str) -> Result<Self, DomainError> {
        options(saved).map(Self)
    }

    /// The saved value of a project or location field.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// Edits the two known fields and retains the rest of the object.
    pub fn apply(mut self, project: &str, location: &str) -> Option<String> {
        let mut changed = false;
        for (key, draft) in [("project_id", project), ("location", location)] {
            if draft == self.text(key).unwrap_or_default() {
                continue;
            }
            changed = true;
            if let Some(value) = super::nonempty(draft) {
                self.0
                    .insert(key.to_owned(), Value::String(value.to_owned()));
            } else {
                self.0.remove(key);
            }
        }
        changed.then(|| Value::Object(self.0).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_schema_edit_preserves_unknown_and_protected_values_and_exact_json_types() {
        let endpoint = ConnectionEndpoint {
            options: r#"{"tree":{"items":[1,null]},"secret":"fixture","extension":7}"#.into(),
            options_schema: r#"{"properties":{"tree":{"type":"object"},"flag":{"type":"boolean"},"secret":{"writeOnly":true}}}"#.into(),
            ..Default::default()
        };
        let editable = || SchemaOptions::from_endpoint(&endpoint).unwrap().unwrap();
        let visible = editable().visible();
        assert_eq!(
            visible,
            json!({"tree": {"items":[1,null]}})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(
            editable()
                .apply(&visible, r#"{"tree":{"items":[1,null]}}"#)
                .unwrap(),
            None
        );
        assert!(
            editable()
                .apply(&visible, r#"{"secret":"replace"}"#)
                .is_err()
        );
        let updated = editable()
            .apply(&visible, r#"{"tree":{"items":[2,null]},"flag":true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&updated).unwrap(),
            json!({"tree":{"items":[2,null]},"flag":true,"secret":"fixture","extension":7})
        );
    }

    #[test]
    fn a_google_edit_preserves_extensions_and_does_not_rewrite_an_unchanged_object() {
        let google =
            || AgentPlatformOptions::load(r#"{"location":"global","extension":[1,null]}"#).unwrap();
        assert_eq!(google().apply("", "global"), None);
        let new = google().apply("project-a", "global").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&new).unwrap(),
            json!({"project_id":"project-a","location":"global","extension":[1,null]})
        );
    }
}
