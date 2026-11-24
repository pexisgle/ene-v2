//! Client declarations, capability grants, and bound device operations.

use crate::Failure;
use crate::ids::CompanionId;
use ene_policy::CapabilitySpec;
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr};

/// Registered client metadata and its granted capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub capabilities: Vec<String>,
    pub granted_capabilities: Vec<String>,
    pub capability_specs: Vec<CapabilitySpec>,
    pub revision: u64,
    pub last_seen: String,
}

impl Client {
    /// Returns `true` if `capability` is currently granted for this client.
    pub fn is_granted(&self, capability: &str) -> bool {
        self.granted_capabilities
            .iter()
            .any(|name| name == capability)
    }

    /// Returns the declared capability spec matching `capability`, if any.
    pub fn spec(&self, capability: &str) -> Option<&CapabilitySpec> {
        self.capability_specs
            .iter()
            .find(|spec| spec.name == capability)
    }

    /// The capabilities that give the parent a tool.
    pub fn tool_specs(&self) -> impl Iterator<Item = &CapabilitySpec> {
        self.capability_specs
            .iter()
            .filter(|spec| !spec.tool_name.is_empty() && self.is_granted(&spec.name))
    }
}

/// Active client binding for one companion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    pub companion_id: CompanionId,
    pub client_id: Option<String>,
}

/// Execution lifecycle status of a queued or dispatched device operation.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DeviceStatus {
    #[default]
    Pending,
    Dispatched,
    Completed,
    Failed,
    Uncertain,
}

impl DeviceStatus {
    /// Returns the canonical snake-case wire/database representation.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Returns `true` once the operation can no longer transition.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Uncertain)
    }
}

/// Persisted record of a capability operation dispatched to a client.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceOperation {
    pub id: String,
    /// The client the host dispatched to, kept for the record.
    pub client_id: String,
    pub capability: String,
    pub input: serde_json::Value,
    pub work_id: Option<String>,
    pub status: DeviceStatus,
    pub output: Option<serde_json::Value>,
    pub failure: Option<Failure>,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, tool_name: &str) -> CapabilitySpec {
        CapabilitySpec {
            name: name.into(),
            tool_name: tool_name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn client_tool_specs_filters_by_grant_and_tool_name() {
        let client = Client {
            granted_capabilities: vec!["body.motion".into(), "eye.watch".into()],
            capability_specs: vec![
                spec("body.motion", "device_motion"),
                spec("eye.watch", ""),
                spec("computer.mouse", "device_mouse"),
            ],
            ..Default::default()
        };
        assert!(client.is_granted("body.motion"));
        assert!(!client.is_granted("computer.mouse"));
        assert_eq!(
            client.spec("computer.mouse").map(|s| s.tool_name.as_str()),
            Some("device_mouse")
        );
        let tools: Vec<_> = client.tool_specs().map(|s| s.name.as_str()).collect();
        assert_eq!(tools, vec!["body.motion"]);
    }

    #[test]
    fn device_status_terminal_and_serde_round_trip() {
        let cases = [
            (DeviceStatus::Pending, "pending", false),
            (DeviceStatus::Dispatched, "dispatched", false),
            (DeviceStatus::Completed, "completed", true),
            (DeviceStatus::Failed, "failed", true),
            (DeviceStatus::Uncertain, "uncertain", true),
        ];
        for (status, expected_str, terminal) in cases {
            assert_eq!(status.as_str(), expected_str);
            assert_eq!(status.is_terminal(), terminal);
            assert_eq!(expected_str.parse::<DeviceStatus>().unwrap(), status);
            let encoded = serde_json::to_string(&status).unwrap();
            assert_eq!(encoded, format!("\"{expected_str}\""));
            let decoded: DeviceStatus = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, status);
        }
    }
}
