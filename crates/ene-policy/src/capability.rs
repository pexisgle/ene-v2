//! Client capability specifications, schema validation and approval targets.
use crate::{DomainError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

/// Prefix every workspace file capability shares.
pub const WORKSPACE_CAPABILITY_PREFIX: &str = "work.";

/// Desktop capability executed by a work agent on its bound client.
pub const COMPUTER_USE: &str = "computer.use";

pub(crate) fn is_shell(name: &str) -> bool {
    matches!(
        name,
        "work.bash" | "work.powershell" | "work.shell" | "device.shell"
    )
}

/// Tools the host itself owns. A client capability may not shadow one, and a
/// plugin may not register a tool with one of these names.
pub const BUILTIN_TOOLS: &[&str] = &[
    "mind_recall",
    "mind_read",
    "mind_update",
    "start_work",
    "send_work",
    "stop_work",
    "device_request",
    "resolve_approval",
    "noop",
    "schedule_create",
    "schedule_list",
    "schedule_cancel",
    "report",
    "web_search",
    "skill_list",
    "skill_read",
    "skill_save",
];

/// How much approval an operation on a capability needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityApproval {
    /// Follow the host's default policy; an unknown capability needs confirmation.
    #[default]
    Unspecified,
    /// Run when policy allows it.
    Auto,
    /// A `Stage::Gate` approval is required for every operation.
    Confirm,
}

/// A capability a client declares, with the JSON Schema the host validates
/// operations against.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySpec {
    /// Capability name, such as `body.motion`; unique per client.
    pub name: String,
    /// Model-facing tool name; empty when the capability exposes no tool.
    #[serde(default)]
    pub tool_name: String,
    /// Model-facing prose describing what the tool does.
    #[serde(default)]
    pub description: String,
    /// JSON Schema object text the host validates each call against.
    #[serde(default = "default_schema")]
    pub input_schema: String,
    /// Named argument presets appended to the description.
    #[serde(default)]
    pub presets: Vec<String>,
    /// Default approval level for operations on this capability.
    #[serde(default)]
    pub approval: CapabilityApproval,
    /// Template for the approval card target, with `{field}` slots.
    #[serde(default)]
    pub approval_target: String,
    /// JSON Schema object text for the result; empty skips validation.
    #[serde(default)]
    pub output_schema: String,
}

fn default_schema() -> String {
    r#"{"type":"object"}"#.into()
}

pub(crate) fn parse_schema_object(text: &str) -> Result<Value> {
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

pub(crate) fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

impl CapabilitySpec {
    /// Checks the name, tool name, schemas and presets this spec declares.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] naming the first field that breaks a rule.
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.contains(char::is_whitespace) {
            return Err(DomainError::Invalid(
                "capability names must be 1-64 non-space characters".into(),
            ));
        }
        if !self.tool_name.is_empty() {
            if self.tool_name.chars().count() > 64
                || self
                    .tool_name
                    .bytes()
                    .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
            {
                return Err(DomainError::Invalid(
                    "capability tool names must be 1-64 [A-Za-z0-9_-] characters".into(),
                ));
            }
            if BUILTIN_TOOLS.contains(&self.tool_name.as_str()) {
                return Err(DomainError::Invalid(format!(
                    "tool name `{}` is reserved by the host",
                    self.tool_name
                )));
            }
        }
        if self.description.chars().count() > 2_000 {
            return Err(DomainError::Invalid(
                "capability description is too long".into(),
            ));
        }
        if self.approval_target.chars().count() > 500 {
            return Err(DomainError::Invalid(
                "capability approval target is too long".into(),
            ));
        }
        let schema = parse_schema_object(&self.input_schema)?;
        if !schema.is_object() {
            return Err(DomainError::Invalid(
                "capability input schema must be an object".into(),
            ));
        }
        if !self.output_schema.trim().is_empty() {
            parse_schema_object(&self.output_schema)?;
        }
        if self.presets.len() > 64
            || self
                .presets
                .iter()
                .any(|preset| preset.chars().count() > 64)
        {
            return Err(DomainError::Invalid(
                "capability presets are too long".into(),
            ));
        }
        Ok(())
    }

    /// The model-facing description, with the presets the client declared.
    pub fn tool_description(&self) -> String {
        let mut description = self.description.clone();
        if !self.presets.is_empty() {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str("Presets: ");
            description.push_str(&self.presets.join(", "));
            description.push('.');
        }
        description
    }
}

/// Validates a whole declaration set: every spec names a declared capability,
/// names are unique and tool names do not collide.
pub fn validate_declarations(capabilities: &[String], specs: &[CapabilitySpec]) -> Result<()> {
    let mut seen = HashSet::new();
    let mut tools = HashSet::new();
    for capability in capabilities {
        if !seen.insert(capability.as_str()) {
            return Err(DomainError::Invalid(format!(
                "capability `{capability}` is declared twice"
            )));
        }
    }
    for spec in specs {
        if !capabilities.iter().any(|name| name == &spec.name) {
            return Err(DomainError::Invalid(format!(
                "capability spec `{}` is not in capabilities",
                spec.name
            )));
        }
        spec.validate()?;
        if !spec.tool_name.is_empty() && !tools.insert(spec.tool_name.as_str()) {
            return Err(DomainError::Invalid(format!(
                "tool name `{}` is declared twice",
                spec.tool_name
            )));
        }
    }
    Ok(())
}

/// Fills the approval-card target from the client's template and the verified
/// input. A template that references a field the input lacks leaves that part
/// empty; the host never trusts a client-supplied target.
pub fn approval_target(spec: &CapabilitySpec, input: &Value) -> String {
    if spec.approval_target.trim().is_empty() {
        return format!("{} {}", spec.name, summarise(input));
    }
    let mut rendered = String::new();
    let mut rest = spec.approval_target.as_str();
    while let Some(start) = rest.find('{') {
        rendered.push_str(&rest[..start]);
        let inner = &rest[start + 1..];
        let Some(end) = inner.find('}') else {
            rendered.push_str(&rest[start..]);
            return rendered;
        };
        let key = &inner[..end];
        let value = input.get(key).and_then(Value::as_str).unwrap_or_default();
        rendered.push_str(value);
        rest = &inner[end + 1..];
    }
    rendered.push_str(rest);
    rendered
}

fn summarise(input: &Value) -> String {
    truncate(&input.to_string(), 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn motion() -> CapabilitySpec {
        CapabilitySpec {
            name: "body.motion".into(),
            tool_name: "body_motion".into(),
            approval: CapabilityApproval::Auto,
            ..Default::default()
        }
    }

    #[test]
    fn approval_targets_expand_verified_input_and_preserve_unclosed_fields() {
        let spec = CapabilitySpec {
            approval_target: "write {path} as {mode}".into(),
            approval: CapabilityApproval::Confirm,
            ..motion()
        };
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a", "mode": "text"})),
            "write /tmp/a as text"
        );
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a"})),
            "write /tmp/a as "
        );
        let spec = CapabilitySpec {
            approval_target: "{path}".into(),
            ..motion()
        };
        assert_eq!(approval_target(&spec, &json!({"path": "/tmp/a"})), "/tmp/a");
        let spec = CapabilitySpec {
            approval_target: "write {path} as {mode".into(),
            ..motion()
        };
        assert_eq!(
            approval_target(&spec, &json!({"path": "/tmp/a", "mode": "text"})),
            "write /tmp/a as {mode"
        );
    }

    #[test]
    fn a_spec_must_name_a_declared_capability_and_cannot_shadow_a_host_tool() {
        assert!(validate_declarations(&["body.motion".into()], &[motion()]).is_ok());
        let shadow = CapabilitySpec {
            tool_name: "mind_recall".into(),
            ..motion()
        };
        assert!(validate_declarations(&["body.motion".into()], &[shadow]).is_err());
    }
}
