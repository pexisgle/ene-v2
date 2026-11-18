//! Task harness inputs and ACP-shaped session events.
//!
//! One harness session carries any number of prompt turns: [`HarnessInput::Start`]
//! creates the session and its first turn, [`HarnessInput::Prompt`] drives a
//! follow-up turn on the live session, and a turn may span several invocations
//! because a Wasm guest cannot suspend inside one. A `request_permission` ends
//! the invocation and the host resumes the turn with
//! [`HarnessInput::Respond`]. `sessionId` is host-owned and never travels over
//! the guest ABI.
pub use agent_client_protocol_schema::v1::{
    Content, ContentBlock, ContentChunk, MessageId, PermissionOption, PermissionOptionId,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use serde::{Deserialize, Serialize};

/// ACP carries arbitrary content; the harness speaks text.
pub fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

pub type RequestId = String;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessInput {
    /// Creates the session, records its working directory and starts the
    /// first prompt turn.
    ///
    /// Equivalent to ACP `session/new` followed by `session/prompt`.
    Start {
        prompt: Vec<ContentBlock>,
        /// The host's own working directory, as ACP `session/new` carries it.
        cwd: String,
    },
    /// Starts a follow-up prompt turn on the live session.
    ///
    /// Equivalent to an ACP `session/prompt` after the first turn ended. Only
    /// the host knows whether a session is live, so a guest that receives
    /// this input owns whatever per-session state it needs to keep it true.
    Prompt { prompt: Vec<ContentBlock> },
    /// Resumes a turn that yielded on [`HarnessEvent::RequestPermission`].
    Respond {
        request_id: RequestId,
        outcome: RequestPermissionOutcome,
    },
    /// Equivalent to the ACP `session/cancel` notification. A turn that is
    /// mid-invocation is terminated by the host without this input ever
    /// reaching the guest; one that is parked on a `request_permission`
    /// receives `Cancel` as its own invocation so it can reset its turn.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessEvent {
    /// Equivalent to the `update` field of an ACP `session/update` notification.
    Update { update: SessionUpdate },
    /// Equivalent to an ACP `session/request_permission` request. Must be the
    /// last event of an invocation; the host answers through `Respond`.
    RequestPermission {
        request_id: RequestId,
        #[serde(rename = "toolCall")]
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    },
    /// Equivalent to the ACP `session/prompt` response.
    Stop {
        #[serde(rename = "stopReason")]
        stop_reason: StopReason,
    },
}
