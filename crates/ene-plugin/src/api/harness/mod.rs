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

    fn event(value: serde_json::Value) -> HarnessEvent {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn acp_v1_tool_call_and_update_payloads_round_trip() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_001",
            "title": "Reading configuration file",
            "name": "read_file",
            "kind": "read",
            "status": "pending"
        });
        let parsed = serde_json::from_value::<SessionUpdate>(call).unwrap();
        let SessionUpdate::ToolCall(call) = parsed else {
            panic!("expected tool call");
        };
        assert_eq!(call.tool_call_id.to_string(), "call_001");
        assert_eq!(call.kind, ToolKind::Read);
        assert_eq!(call.status, ToolCallStatus::Pending);

        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_001",
            "status": "in_progress",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "Found 3 configuration files..."}
            }],
            "locations": [{"path": "/workspace/ene.toml", "line": 4}]
        });
        let parsed = serde_json::from_value::<SessionUpdate>(update.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), update);
    }

    #[test]
    fn acp_v1_message_chunk_and_plan_payloads_round_trip() {
        let chunk = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"},
            "messageId": "message-1"
        });
        let parsed = serde_json::from_value::<SessionUpdate>(chunk.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), chunk);

        let plan = json!({
            "sessionUpdate": "plan",
            "entries": [{
                "content": "Inspect the workspace",
                "priority": "high",
                "status": "in_progress"
            }]
        });
        let parsed = serde_json::from_value::<SessionUpdate>(plan.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), plan);
    }

    #[test]
    fn permission_request_and_response_follow_the_acp_v1_shape() {
        let permission = json!({
            "type": "request_permission",
            "request_id": "permission-1",
            "toolCall": {"toolCallId": "call_001", "status": "pending"},
            "options": [{
                "optionId": "allow-once",
                "name": "Allow once",
                "kind": "allow_once"
            }, {
                "optionId": "reject-once",
                "name": "Reject",
                "kind": "reject_once"
            }]
        });
        let parsed = event(permission.clone());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), permission);

        let selected = json!({
            "type": "respond",
            "request_id": "permission-1",
            "outcome": {"outcome": "selected", "optionId": "allow-once"}
        });
        let parsed = serde_json::from_value::<HarnessInput>(selected.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), selected);

        let cancelled = json!({"outcome": "cancelled"});
        let value = serde_json::from_value::<RequestPermissionOutcome>(cancelled.clone()).unwrap();
        assert_eq!(serde_json::to_value(&value).unwrap(), cancelled);
    }

    #[test]
    fn prompt_cancel_and_stop_follow_the_acp_v1_shape() {
        let start = json!({"type": "start", "prompt": [{"type": "text", "text": "調べる"}]});
        let parsed = serde_json::from_value::<HarnessInput>(start.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), start);

        assert_eq!(
            serde_json::to_value(HarnessInput::Cancel).unwrap(),
            json!({"type": "cancel"})
        );
        assert_eq!(
            serde_json::to_value(event(json!({"type": "stop", "stopReason": "end_turn"}))).unwrap(),
            json!({"type": "stop", "stopReason": "end_turn"})
        );
    }

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
