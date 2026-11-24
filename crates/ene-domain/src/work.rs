//! Work session and run lifecycle domain models.

use crate::Failure;
use crate::ids::CompanionId;
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr};

/// Overall lifecycle state of a work session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorkStatus {
    Idle,
    Active,
    Closed,
}

impl WorkStatus {
    /// Returns the canonical snake-case wire/database representation.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Execution state of an individual instruction run inside a work session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    WaitingInput,
    Completed,
    /// A stop reached its final state. A stored row that still spells a state
    /// this build no longer has decodes here.
    #[serde(alias = "cancelling", alias = "cancellation_unconfirmed")]
    Cancelled,
    Failed,
}

impl RunState {
    /// Returns the canonical snake-case wire/database representation.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Returns `true` once the run has finished and emits no further events.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

/// Persistent session container for commissioned work ("hands").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSession {
    pub id: String,
    pub companion_id: CompanionId,
    pub title: String,
    pub status: WorkStatus,
    /// The client the work was commissioned from. Empty when no client was
    /// active at commissioning.
    pub client_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Persistent record of one instruction turn inside a work session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub run_id: String,
    pub work_id: String,
    pub instruction: String,
    pub state: RunState,
    /// The approval the run is waiting on, while it is `WaitingInput`.
    pub pending_request_id: Option<String>,
    pub failure: Option<Failure>,
    pub started_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_status_and_run_state_round_trip_and_decode_legacy_aliases() {
        for (status, name) in [
            (WorkStatus::Idle, "idle"),
            (WorkStatus::Active, "active"),
            (WorkStatus::Closed, "closed"),
        ] {
            assert_eq!(status.as_str(), name);
            assert_eq!(name.parse::<WorkStatus>().unwrap(), status);
            let encoded = serde_json::to_string(&status).unwrap();
            assert_eq!(encoded, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<WorkStatus>(&encoded).unwrap(),
                status
            );
        }

        let cases = [
            (RunState::Starting, "starting", false),
            (RunState::Running, "running", false),
            (RunState::WaitingInput, "waiting_input", false),
            (RunState::Completed, "completed", true),
            (RunState::Cancelled, "cancelled", true),
            (RunState::Failed, "failed", true),
        ];
        for (state, name, terminal) in cases {
            assert_eq!(state.as_str(), name);
            assert_eq!(name.parse::<RunState>().unwrap(), state);
            assert_eq!(state.is_terminal(), terminal);
            let encoded = serde_json::to_string(&state).unwrap();
            assert_eq!(encoded, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<RunState>(&encoded).unwrap(), state);
        }

        for alias in ["cancelling", "cancellation_unconfirmed"] {
            let decoded: RunState = serde_json::from_str(&format!("\"{alias}\"")).unwrap();
            assert_eq!(decoded, RunState::Cancelled);
        }
    }
}
