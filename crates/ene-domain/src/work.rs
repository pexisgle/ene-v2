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
    /// The session exists but no run is in flight.
    Idle,
    /// A run is in flight.
    Active,
    /// The session is finished and takes no further runs.
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
    /// Accepted but not yet dispatched to the agent.
    Starting,
    /// The agent is working on the instruction.
    Running,
    /// Blocked on an approval the run cannot proceed without.
    WaitingInput,
    /// The agent finished and reported.
    Completed,
    /// A stop reached its final state. A stored row that still spells a state
    /// this build no longer has decodes here.
    #[serde(alias = "cancelling", alias = "cancellation_unconfirmed")]
    Cancelled,
    /// The agent stopped with an error; see `Run::failure`.
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
    /// Row id of this session.
    pub id: String,
    /// The companion the work is commissioned for.
    pub companion_id: CompanionId,
    /// Operator-facing title.
    pub title: String,
    /// Where the session is in its lifecycle.
    pub status: WorkStatus,
    /// The client the work was commissioned from. Empty when no client was
    /// active at commissioning.
    pub client_id: Option<String>,
    /// Creation time, UTC RFC 3339 at whole-second precision.
    pub created_at: String,
    /// Last update, UTC RFC 3339 at whole-second precision.
    pub updated_at: String,
}

/// Persistent record of one instruction turn inside a work session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Row id of this run.
    pub run_id: String,
    /// The session the run belongs to.
    pub work_id: String,
    /// The instruction the agent was given.
    pub instruction: String,
    /// Where the run is in its lifecycle.
    pub state: RunState,
    /// The current pending approval projected by the repository while waiting for input.
    pub pending_request_id: Option<String>,
    /// Why the run failed, when it did.
    pub failure: Option<Failure>,
    /// Start time, UTC RFC 3339 at whole-second precision.
    pub started_at: String,
    /// Last update, UTC RFC 3339 at whole-second precision.
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
