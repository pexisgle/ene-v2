//! Client declarations, capability grants, and bound device operations.

use crate::Failure;
use crate::ids::CompanionId;
use ene_policy::{COMPUTER_USE, CapabilitySpec, WORKSPACE_CAPABILITY_PREFIX};
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr};

/// Registered client metadata and its granted capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    /// Client-assigned id, unique across registered clients.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Platform the client runs on, as the client reports it.
    pub platform: String,
    /// Capabilities the client declared at registration.
    pub capabilities: Vec<String>,
    /// Declared capabilities authorized by the host's current policy.
    pub granted_capabilities: Vec<String>,
    /// Schema and approval policy for each declared capability.
    pub capability_specs: Vec<CapabilitySpec>,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Last contact, UTC RFC 3339 at whole-second precision.
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
        self.capability_specs.iter().filter(|spec| {
            !spec.tool_name.is_empty()
                && spec.name != COMPUTER_USE
                && !spec.name.starts_with(WORKSPACE_CAPABILITY_PREFIX)
                && self.is_granted(&spec.name)
        })
    }
}

/// Active client binding for one companion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    /// The companion the client is bound to.
    pub companion_id: CompanionId,
    /// The bound client, or `None` when no client is active.
    pub client_id: Option<String>,
    /// Monotonic generation on the host, omitted from the public wire view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// Execution lifecycle status of a queued or dispatched device operation.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DeviceStatus {
    /// Queued, not yet sent to the client.
    #[default]
    Pending,
    /// Sent to the client, outcome unknown.
    Dispatched,
    /// The client reported success.
    Completed,
    /// The operation failed, or the client reported a failure.
    Failed,
    /// Dispatch may have reached the client but no result came back.
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

/// The agent lifetime a pending device operation is authorized to start in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceScope {
    /// A companion's assignment to the target client.
    Companion {
        /// Companion that owns the operation.
        companion_id: CompanionId,
        /// The accepted binding, absent in older records.
        activation_generation: Option<u64>,
    },
    /// A particular instruction run on the work session's bound client.
    Work {
        /// Companion that owns the work session.
        companion_id: CompanionId,
        /// Session that binds this operation to its client.
        work_id: String,
        /// Instruction run that owns this operation.
        run_id: String,
    },
    /// An older recorded operation whose owner could not be established.
    Legacy {
        /// Session recorded by the older host, when it was a work operation.
        work_id: Option<String>,
    },
}

impl DeviceScope {
    /// The addressed companion, when the host recorded it.
    pub fn companion_id(&self) -> Option<&CompanionId> {
        match self {
            Self::Companion { companion_id, .. } | Self::Work { companion_id, .. } => {
                Some(companion_id)
            }
            Self::Legacy { .. } => None,
        }
    }

    /// The persisted session identifier, if part of work.
    pub fn work_id(&self) -> Option<&str> {
        match self {
            Self::Work { work_id, .. } => Some(work_id),
            Self::Legacy { work_id } => work_id.as_deref(),
            Self::Companion { .. } => None,
        }
    }

    /// The owning instruction run, if bound to one.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            Self::Work { run_id, .. } => Some(run_id),
            _ => None,
        }
    }
}

/// Persisted record of a capability operation dispatched to a client.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceOperation {
    /// Row id of this operation.
    pub id: String,
    /// The client the host dispatched to, kept for the record.
    pub client_id: String,
    /// The capability the operation invokes.
    pub capability: String,
    /// Arguments the host validated and sent.
    pub input: serde_json::Value,
    /// The lifetime and authority that own the operation.
    pub scope: DeviceScope,
    /// Where the operation is in its lifecycle.
    pub status: DeviceStatus,
    /// The client result, once it reported success.
    pub output: Option<serde_json::Value>,
    /// Why the operation failed, when it did.
    pub failure: Option<Failure>,
    /// Creation time, UTC RFC 3339 at whole-second precision.
    pub created_at: String,
    /// Last update, UTC RFC 3339 at whole-second precision.
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
            granted_capabilities: vec![
                "body.motion".into(),
                "eye.watch".into(),
                "work.write".into(),
            ],
            capability_specs: vec![
                spec("body.motion", "device_motion"),
                spec("eye.watch", ""),
                spec("computer.mouse", "device_mouse"),
                spec("work.write", "device_write"),
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
