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

/// Identifier the host assigns to one `request_permission`.
pub type RequestId = String;

/// A request from the host that advances the harness turn.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessInput {
    /// Creates the session, records its working directory and starts the
    /// first prompt turn.
    ///
    /// Equivalent to ACP `session/new` followed by `session/prompt`.
    Start {
        /// The turn's initial content.
        prompt: Vec<ContentBlock>,
        /// The host's own working directory, as ACP `session/new` carries it.
        cwd: String,
    },
    /// Starts a follow-up prompt turn on the live session.
    ///
    /// Equivalent to an ACP `session/prompt` after the first turn ended. Only
    /// the host knows whether a session is live, so a guest that receives
    /// this input owns whatever per-session state it needs to keep it true.
    Prompt {
        /// The turn's content.
        prompt: Vec<ContentBlock>,
    },
    /// Resumes a turn that yielded on [`HarnessEvent::RequestPermission`].
    Respond {
        /// The request being answered.
        request_id: RequestId,
        /// What the person chose.
        outcome: RequestPermissionOutcome,
    },
    /// Equivalent to the ACP `session/cancel` notification. A turn that is
    /// mid-invocation is terminated by the host without this input ever
    /// reaching the guest; one that is parked on a `request_permission`
    /// receives `Cancel` as its own invocation so it can reset its turn.
    Cancel,
}

/// A step of the harness turn reported by the guest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessEvent {
    /// Equivalent to the `update` field of an ACP `session/update` notification.
    Update {
        /// The update to report.
        update: SessionUpdate,
    },
    /// Equivalent to an ACP `session/request_permission` request. Must be the
    /// last event of an invocation; the host answers through `Respond`.
    RequestPermission {
        /// Id the host answers this request with.
        request_id: RequestId,
        /// The tool call the person is being asked about.
        #[serde(rename = "toolCall")]
        tool_call: ToolCallUpdate,
        /// The choices to offer.
        options: Vec<PermissionOption>,
    },
    /// Equivalent to the ACP `session/prompt` response.
    Stop {
        /// Why the turn ended.
        #[serde(rename = "stopReason")]
        stop_reason: StopReason,
    },
}
