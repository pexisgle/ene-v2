//! Task harness inputs and ACP-shaped session events.
//!
//! The host drives one prompt turn per session. A Wasm guest cannot suspend
//! inside an invocation, so a `request_permission` ends the invocation and the
//! host resumes it with [`HarnessInput::Respond`]. `sessionId` is host-owned
//! and never travels over the guest ABI.
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
    /// Creates the session and starts its single prompt turn.
    ///
    /// Equivalent to ACP `session/new` followed by `session/prompt`.
    Start { prompt: Vec<ContentBlock> },
    /// Resumes a turn that yielded on [`HarnessEvent::RequestPermission`].
    Respond {
        request_id: RequestId,
        outcome: RequestPermissionOutcome,
    },
    /// Equivalent to the ACP `session/cancel` notification. The host terminates
    /// the turn directly and never forwards this input to the guest.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payload_fields_may_grow_but_the_envelope_stays_strict() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_001",
            "title": "Reading configuration file",
            "_meta": {"future": true}
        });
        serde_json::from_value::<SessionUpdate>(call).unwrap();

        assert!(
            serde_json::from_value::<HarnessEvent>(json!({
                "type": "stop",
                "stopReason": "end_turn",
                "extra": true
            }))
            .is_err()
        );
        // The schema owns the update type; the runtime rejects variants outside
        // the harness contract.
        assert!(
            serde_json::from_value::<HarnessEvent>(json!({
                "type": "update",
                "update": {"sessionUpdate": "unknown_update"}
            }))
            .is_err()
        );
        // Only text content blocks are part of the harness contract. The ACP
        // payload accepts other variants; the runtime rejects them.
        serde_json::from_value::<HarnessEvent>(json!({
            "type": "update",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "image", "data": "AA==", "mimeType": "image/png"}
            }
        }))
        .unwrap();
    }
}
