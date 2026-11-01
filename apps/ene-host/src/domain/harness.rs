//! Task session state expressed in ACP v1 vocabulary.
//!
//! The Wasm bridge and a future external ACP adapter both drive this state
//! machine; it never depends on Extism, storage or the Connect API.
use super::{Error, ErrorKind};
use ene_plugin::{
    ContentBlock, HarnessEvent, PermissionOption, Plan, RequestPermissionOutcome, SessionUpdate,
    StopReason, ToolCall, ToolCallStatus, ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONTRACT_VERSION: u32 = ene_plugin::CONTRACT_VERSION;
/// Transcript entries kept for the task view; the newest entries survive.
pub const MAX_TRANSCRIPT: usize = 256;
/// User-visible report accumulated from `agent_message_chunk` text.
pub const MAX_REPORT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingForInput => "waiting_for_input",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingPermission {
    pub request_id: String,
    #[serde(rename = "toolCall")]
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub contract_version: u32,
    pub session_id: String,
    pub status: Status,
    #[serde(default)]
    pub pending: Option<PendingPermission>,
    #[serde(default)]
    pub tool_calls: BTreeMap<String, ToolCall>,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub report: String,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub events: Vec<HarnessEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Updated,
    Waiting,
    Stopped,
}

impl Session {
    pub fn start(session_id: impl Into<String>) -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            session_id: session_id.into(),
            status: Status::Running,
            pending: None,
            tool_calls: BTreeMap::new(),
            plan: None,
            message_id: None,
            report: String::new(),
            stop_reason: None,
            failure: None,
            events: Vec::new(),
        }
    }
    /// A session that cannot be resumed, used when a stored journal predates
    /// the current contract or no longer parses.
    pub fn interrupted(session_id: impl Into<String>) -> Self {
        let mut session = Self::start(session_id);
        session.fail("host_interrupted");
        session
    }
    pub fn terminal(&self) -> bool {
        self.status.terminal()
    }
    pub fn apply(&mut self, event: &HarnessEvent) -> Result<Change> {
        if self.status.terminal() {
            return Err(conflict());
        }
        let change = match event {
            HarnessEvent::Update { update } => {
                self.apply_update(update)?;
                Change::Updated
            }
            HarnessEvent::RequestPermission {
                request_id,
                tool_call,
                options,
            } => {
                if self.pending.is_some() || tool_call.tool_call_id.is_empty() {
                    return Err(invalid());
                }
                // ACP carries display details on the request itself; an agent
                // is not required to announce the call first.
                if !self.tool_calls.contains_key(&tool_call.tool_call_id) {
                    self.tool_calls.insert(
                        tool_call.tool_call_id.clone(),
                        ToolCall {
                            tool_call_id: tool_call.tool_call_id.clone(),
                            title: tool_call.title.clone().unwrap_or_else(|| "tool".into()),
                            kind: tool_call.kind,
                            status: tool_call.status.unwrap_or_default(),
                            ..Default::default()
                        },
                    );
                }
                self.patch_tool_call(tool_call)?;
                self.pending = Some(PendingPermission {
                    request_id: request_id.clone(),
                    tool_call: tool_call.clone(),
                    options: options.clone(),
                });
                self.status = Status::WaitingForInput;
                Change::Waiting
            }
            HarnessEvent::Stop { stop_reason } => {
                if self.pending.is_some() {
                    return Err(conflict());
                }
                self.stop(*stop_reason);
                Change::Stopped
            }
        };
        self.events.push(event.clone());
        self.trim();
        Ok(change)
    }
    /// Clears the waiting permission and allows the guest to resume.
    pub fn respond(&mut self, request_id: &str, outcome: &RequestPermissionOutcome) -> Result<()> {
        if self.status != Status::WaitingForInput {
            return Err(conflict());
        }
        let pending = self.pending.as_ref().ok_or_else(conflict)?;
        if pending.request_id != request_id {
            return Err(conflict());
        }
        if let RequestPermissionOutcome::Selected { option_id } = outcome
            && !pending
                .options
                .iter()
                .any(|option| &option.option_id == option_id)
        {
            return Err(invalid());
        }
        self.pending = None;
        self.status = Status::Running;
        Ok(())
    }
    /// Host-owned cancellation; idempotent for an already cancelled session.
    pub fn cancel(&mut self) {
        if self.terminal() {
            return;
        }
        self.pending = None;
        self.status = Status::Cancelled;
        self.stop_reason = Some(StopReason::Cancelled);
        self.events.push(HarnessEvent::Stop {
            stop_reason: StopReason::Cancelled,
        });
        self.trim();
    }
    /// Host-owned failure; a terminated session keeps its verified history.
    pub fn fail(&mut self, reason: &str) {
        if self.terminal() {
            return;
        }
        self.pending = None;
        self.status = Status::Failed;
        self.failure = Some(reason.into());
    }
    fn stop(&mut self, stop_reason: StopReason) {
        self.pending = None;
        self.stop_reason = Some(stop_reason);
        self.status = match stop_reason {
            StopReason::EndTurn => Status::Completed,
            StopReason::Cancelled => Status::Cancelled,
            StopReason::MaxTokens | StopReason::MaxTurnRequests | StopReason::Refusal => {
                self.failure = Some(stop_name(stop_reason).into());
                Status::Failed
            }
        };
    }
    fn apply_update(&mut self, update: &SessionUpdate) -> Result<()> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => self.append_text(chunk),
            SessionUpdate::AgentThoughtChunk(_) => Ok(()),
            SessionUpdate::ToolCall(call) => {
                if call.tool_call_id.is_empty() || self.tool_calls.contains_key(&call.tool_call_id)
                {
                    return Err(invalid());
                }
                self.tool_calls
                    .insert(call.tool_call_id.clone(), call.clone());
                Ok(())
            }
            SessionUpdate::ToolCallUpdate(update) => self.patch_tool_call(update),
            SessionUpdate::Plan(plan) => {
                self.plan = Some(plan.clone());
                Ok(())
            }
        }
    }
    fn append_text(&mut self, chunk: &ene_plugin::ContentChunk) -> Result<()> {
        let ContentBlock::Text(text) = &chunk.content;
        if chunk.message_id.is_some() && chunk.message_id != self.message_id {
            self.message_id = chunk.message_id.clone();
            self.report.clear();
        }
        if self.report.len() + text.text.len() > MAX_REPORT_BYTES {
            return Err(Error::new(ErrorKind::ResourceLimit));
        }
        self.report.push_str(&text.text);
        Ok(())
    }
    fn patch_tool_call(&mut self, update: &ToolCallUpdate) -> Result<()> {
        let call = self
            .tool_calls
            .get_mut(&update.tool_call_id)
            .ok_or_else(invalid)?;
        if let Some(status) = update.status {
            if !advances(call.status, status) {
                return Err(invalid());
            }
            call.status = status;
        }
        if let Some(title) = &update.title {
            call.title = title.clone();
        }
        if let Some(name) = &update.name {
            call.name = Some(name.clone());
        }
        if let Some(kind) = update.kind {
            call.kind = Some(kind);
        }
        if let Some(content) = &update.content {
            call.content = content.clone();
        }
        if let Some(locations) = &update.locations {
            call.locations = locations.clone();
        }
        if let Some(raw) = &update.raw_input {
            call.raw_input = Some(raw.clone());
        }
        if let Some(raw) = &update.raw_output {
            call.raw_output = Some(raw.clone());
        }
        Ok(())
    }
    fn trim(&mut self) {
        if self.events.len() > MAX_TRANSCRIPT {
            self.events.drain(..self.events.len() - MAX_TRANSCRIPT);
        }
    }
}

fn advances(from: ToolCallStatus, to: ToolCallStatus) -> bool {
    match from {
        ToolCallStatus::Pending => matches!(
            to,
            ToolCallStatus::Pending
                | ToolCallStatus::InProgress
                | ToolCallStatus::Completed
                | ToolCallStatus::Failed
        ),
        ToolCallStatus::InProgress => matches!(
            to,
            ToolCallStatus::InProgress | ToolCallStatus::Completed | ToolCallStatus::Failed
        ),
        ToolCallStatus::Completed => to == ToolCallStatus::Completed,
        ToolCallStatus::Failed => to == ToolCallStatus::Failed,
    }
}

fn stop_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
    }
}

fn invalid() -> Error {
    Error::new(ErrorKind::InvalidRequest)
}

fn conflict() -> Error {
    Error::new(ErrorKind::StateConflict)
}

type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use ene_plugin::{
        ContentChunk, PermissionOptionKind, PlanEntry, PlanEntryPriority, PlanEntryStatus,
        ToolCallContent, ToolKind,
    };

    fn message(id: Option<&str>, text: &str) -> HarnessEvent {
        HarnessEvent::Update {
            update: SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::text(text),
                message_id: id.map(str::to_owned),
            }),
        }
    }

    fn tool_call(id: &str) -> HarnessEvent {
        HarnessEvent::Update {
            update: SessionUpdate::ToolCall(ToolCall {
                tool_call_id: id.into(),
                title: "Read the workspace".into(),
                kind: Some(ToolKind::Read),
                ..Default::default()
            }),
        }
    }

    fn permission(id: &str, call: &str, allow: bool) -> HarnessEvent {
        HarnessEvent::RequestPermission {
            request_id: id.into(),
            tool_call: ToolCallUpdate {
                tool_call_id: call.into(),
                ..Default::default()
            },
            options: vec![PermissionOption {
                option_id: if allow { "allow" } else { "deny" }.into(),
                name: "answer".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        }
    }

    fn new_session() -> Session {
        Session::start("session-1")
    }

    #[test]
    fn report_uses_the_current_message_id() {
        let mut session = new_session();
        session
            .apply(&message(Some("progress"), "checking"))
            .unwrap();
        session.apply(&message(Some("progress"), " again")).unwrap();
        session.apply(&message(Some("report"), "done")).unwrap();
        assert_eq!(session.report, "done");
        assert_eq!(session.message_id.as_deref(), Some("report"));
    }

    #[test]
    fn tool_calls_are_upserted_and_status_regressions_are_rejected() {
        let mut session = new_session();
        session.apply(&tool_call("call-1")).unwrap();
        session
            .apply(&HarnessEvent::Update {
                update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: "call-1".into(),
                    status: Some(ToolCallStatus::InProgress),
                    content: Some(vec![ToolCallContent::Content {
                        content: ContentBlock::text("partial"),
                    }]),
                    ..Default::default()
                }),
            })
            .unwrap();
        session
            .apply(&HarnessEvent::Update {
                update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: "call-1".into(),
                    status: Some(ToolCallStatus::Completed),
                    ..Default::default()
                }),
            })
            .unwrap();
        assert_eq!(
            session.tool_calls["call-1"].status,
            ToolCallStatus::Completed
        );
        assert!(
            session
                .apply(&HarnessEvent::Update {
                    update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: "call-1".into(),
                        status: Some(ToolCallStatus::InProgress),
                        ..Default::default()
                    }),
                })
                .is_err()
        );
        assert!(
            session
                .apply(&HarnessEvent::Update {
                    update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                        tool_call_id: "unknown".into(),
                        ..Default::default()
                    }),
                })
                .is_err()
        );
    }

    #[test]
    fn a_permission_request_synthesizes_a_missing_tool_call() {
        let mut session = new_session();
        session
            .apply(&permission("permission-1", "missing", true))
            .unwrap();
        assert!(session.tool_calls.contains_key("missing"));
        assert_eq!(session.status, Status::WaitingForInput);
        let mut empty = new_session();
        assert!(empty.apply(&permission("permission-1", "", true)).is_err());
    }

    #[test]
    fn a_response_requires_a_waiting_request() {
        let mut session = new_session();
        assert!(
            session
                .respond(
                    "permission-1",
                    &RequestPermissionOutcome::Selected {
                        option_id: "allow".into()
                    }
                )
                .is_err()
        );
        session.apply(&tool_call("call-1")).unwrap();
        session
            .apply(&permission("permission-1", "call-1", true))
            .unwrap();
        assert!(
            session
                .respond(
                    "permission-2",
                    &RequestPermissionOutcome::Selected {
                        option_id: "allow".into()
                    }
                )
                .is_err()
        );
        assert_eq!(session.status, Status::WaitingForInput);
    }

    #[test]
    fn a_permission_waits_for_a_known_option_and_resumes_running() {
        let mut session = new_session();
        session.apply(&tool_call("call-1")).unwrap();
        assert_eq!(
            session
                .apply(&permission("permission-1", "call-1", true))
                .unwrap(),
            Change::Waiting
        );
        assert_eq!(session.status, Status::WaitingForInput);
        assert!(
            session
                .respond(
                    "permission-1",
                    &RequestPermissionOutcome::Selected {
                        option_id: "other".into()
                    }
                )
                .is_err()
        );
        session
            .respond(
                "permission-1",
                &RequestPermissionOutcome::Selected {
                    option_id: "allow".into(),
                },
            )
            .unwrap();
        assert_eq!(session.status, Status::Running);
        assert!(session.pending.is_none());
    }

    #[test]
    fn stop_reasons_map_to_task_outcomes() {
        for (reason, status, failure) in [
            (StopReason::EndTurn, Status::Completed, None),
            (StopReason::Cancelled, Status::Cancelled, None),
            (StopReason::MaxTokens, Status::Failed, Some("max_tokens")),
            (
                StopReason::MaxTurnRequests,
                Status::Failed,
                Some("max_turn_requests"),
            ),
            (StopReason::Refusal, Status::Failed, Some("refusal")),
        ] {
            let mut session = new_session();
            session
                .apply(&HarnessEvent::Stop {
                    stop_reason: reason,
                })
                .unwrap();
            assert_eq!(session.status, status);
            assert_eq!(session.failure.as_deref(), failure);
            assert!(session.terminal());
        }
    }

    #[test]
    fn cancel_and_fail_are_terminal_and_idempotent() {
        let mut session = new_session();
        session.cancel();
        let events = session.events.len();
        session.cancel();
        assert_eq!(session.events.len(), events);
        assert_eq!(session.status, Status::Cancelled);
        assert_eq!(session.stop_reason, Some(StopReason::Cancelled));
        assert!(session.apply(&message(None, "late")).is_err());

        let mut failed = new_session();
        failed.fail("plugin_stopped");
        failed.fail("other");
        assert_eq!(failed.failure.as_deref(), Some("plugin_stopped"));
    }

    #[test]
    fn transcript_keeps_the_newest_entries() {
        let mut session = new_session();
        for index in 0..MAX_TRANSCRIPT + 10 {
            session.apply(&message(None, &index.to_string())).unwrap();
        }
        assert_eq!(session.events.len(), MAX_TRANSCRIPT);
        // The report is accumulated while streaming, independently of the
        // transcript window used for the task view.
        assert!(session.report.starts_with('0'));
        assert!(session.report.ends_with(&(MAX_TRANSCRIPT + 9).to_string()));
    }

    #[test]
    fn the_report_is_bounded() {
        let mut session = new_session();
        assert!(
            session
                .apply(&message(None, &"x".repeat(MAX_REPORT_BYTES + 1)))
                .is_err()
        );
        assert!(session.report.is_empty());
    }

    #[test]
    fn plans_replace_the_previous_plan() {
        let mut session = new_session();
        let plan = |status| Plan {
            entries: vec![PlanEntry {
                content: "step".into(),
                priority: PlanEntryPriority::High,
                status,
            }],
        };
        session
            .apply(&HarnessEvent::Update {
                update: SessionUpdate::Plan(plan(PlanEntryStatus::Pending)),
            })
            .unwrap();
        session
            .apply(&HarnessEvent::Update {
                update: SessionUpdate::Plan(plan(PlanEntryStatus::Completed)),
            })
            .unwrap();
        assert_eq!(
            session.plan.as_ref().unwrap().entries[0].status,
            PlanEntryStatus::Completed
        );
    }
}
