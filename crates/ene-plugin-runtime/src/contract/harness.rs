//! ACP-shaped harness event validation.
use crate::{Error, Result, limits::MAX_CHUNK};
use ene_plugin::{
    ContentBlock, HarnessEvent, PermissionOption, Plan, SessionUpdate, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallUpdate,
};

const MAX_IDENTIFIER: usize = 128;
const MAX_TITLE: usize = 256;
const MAX_PATH: usize = 4096;
const MAX_OPTIONS: usize = 8;
const MAX_TOOL_CONTENT: usize = 64;
const MAX_TOOL_LOCATIONS: usize = 64;
const MAX_RAW_BYTES: usize = 64 * 1024;
const MAX_PLAN_ENTRIES: usize = 512;
const MAX_PLAN_TEXT: usize = 2048;

pub(super) enum Progress {
    Update,
    AwaitingPermission,
    Stopped,
}

pub(super) fn validate(event: &HarnessEvent) -> Result<Progress> {
    match event {
        HarnessEvent::Update { update } => validate_update(update).map(|()| Progress::Update),
        HarnessEvent::RequestPermission {
            request_id,
            tool_call,
            options,
        } => {
            identifier(request_id.as_str())?;
            validate_tool_update(tool_call)?;
            validate_options(options)?;
            Ok(Progress::AwaitingPermission)
        }
        HarnessEvent::Stop { .. } => Ok(Progress::Stopped),
    }
}

fn validate_update(update: &SessionUpdate) -> Result<()> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(message) = &chunk.message_id {
                identifier(message.0.as_ref())?;
            }
            text(&chunk.content)
        }
        SessionUpdate::ToolCall(call) => validate_tool_call(call),
        SessionUpdate::ToolCallUpdate(update) => validate_tool_update(update),
        SessionUpdate::Plan(plan) => validate_plan(plan),
        // The schema type is wider than the harness contract; the other
        // session-update kinds are not part of the guest ABI.
        _ => Err(Error::Protocol),
    }
}

fn validate_tool_call(call: &ToolCall) -> Result<()> {
    identifier(call.tool_call_id.0.as_ref())?;
    bounded(&call.title, MAX_TITLE)?;
    if let Some(name) = &call.name {
        bounded(name, MAX_IDENTIFIER)?;
    }
    content(&call.content)?;
    locations(call.locations.iter())?;
    raw(call.raw_input.as_ref())?;
    raw(call.raw_output.as_ref())
}

fn validate_tool_update(update: &ToolCallUpdate) -> Result<()> {
    identifier(update.tool_call_id.0.as_ref())?;
    if let Some(title) = &update.fields.title {
        bounded(title, MAX_TITLE)?;
    }
    if let Some(name) = &update.fields.name {
        bounded(name, MAX_IDENTIFIER)?;
    }
    if let Some(items) = &update.fields.content {
        content(items)?;
    }
    if let Some(paths) = &update.fields.locations {
        locations(paths.iter())?;
    }
    raw(update.fields.raw_input.as_ref())?;
    raw(update.fields.raw_output.as_ref())
}

fn validate_plan(plan: &Plan) -> Result<()> {
    if plan.entries.len() > MAX_PLAN_ENTRIES {
        return Err(Error::Protocol);
    }
    for entry in &plan.entries {
        bounded(&entry.content, MAX_PLAN_TEXT)?;
    }
    Ok(())
}

fn validate_options(options: &[PermissionOption]) -> Result<()> {
    if options.is_empty() || options.len() > MAX_OPTIONS {
        return Err(Error::Protocol);
    }
    let mut ids = std::collections::HashSet::new();
    for option in options {
        identifier(option.option_id.0.as_ref())?;
        bounded(&option.name, MAX_TITLE)?;
        if !ids.insert(&option.option_id) {
            return Err(Error::Protocol);
        }
    }
    Ok(())
}

fn content(items: &[ToolCallContent]) -> Result<()> {
    if items.len() > MAX_TOOL_CONTENT {
        return Err(Error::Protocol);
    }
    for item in items {
        match item {
            ToolCallContent::Content(content) => text(&content.content)?,
            _ => return Err(Error::Protocol),
        }
    }
    Ok(())
}

fn locations<'a>(paths: impl ExactSizeIterator<Item = &'a ToolCallLocation>) -> Result<()> {
    if paths.len() > MAX_TOOL_LOCATIONS {
        return Err(Error::Protocol);
    }
    for location in paths {
        bounded(location.path.to_str().ok_or(Error::Protocol)?, MAX_PATH)?;
    }
    Ok(())
}

fn text(block: &ContentBlock) -> Result<()> {
    match block {
        ContentBlock::Text(text) => bounded(&text.text, MAX_CHUNK),
        _ => Err(Error::Protocol),
    }
}

struct RawSize(usize);

impl std::io::Write for RawSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("raw field too large"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn raw(value: Option<&serde_json::Value>) -> Result<()> {
    if let Some(value) = value {
        serde_json::to_writer(&mut RawSize(MAX_RAW_BYTES), value).map_err(|_| Error::Protocol)?;
    }
    Ok(())
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER || value.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    Ok(())
}

fn bounded(value: &str, limit: usize) -> Result<()> {
    if value.len() > limit {
        return Err(Error::Protocol);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Events;
    use ene_plugin::{
        ContentChunk, HarnessInput, Operation, PermissionOptionKind, StopReason, ToolCall,
        ToolCallLocation, ToolCallStatus, ToolCallUpdateFields, text_block,
    };

    fn events() -> Events {
        Events::new(&Operation::Harness {
            input: HarnessInput::Prompt { prompt: vec![] },
        })
    }

    fn option(id: &str) -> PermissionOption {
        PermissionOption::new(id.to_owned(), "Allow", PermissionOptionKind::AllowOnce)
    }

    fn permission(request_id: &str, options: Vec<PermissionOption>) -> HarnessEvent {
        HarnessEvent::RequestPermission {
            request_id: request_id.into(),
            tool_call: ToolCallUpdate::new("call-1", ToolCallUpdateFields::new()),
            options,
        }
    }

    fn accept(events: &mut Events, event: HarnessEvent) -> Result<()> {
        events.accept(&ene_plugin::Event::Harness { event })
    }

    #[test]
    fn raw_size_matches_json_bytes_including_escaping_and_unicode() {
        use serde_json::json;
        for value in [
            json!(null),
            json!(true),
            json!(-12),
            json!(u64::MAX),
            json!(1.25),
            json!("雪🦀"),
            json!("\"\\\n\u{0000}"),
            json!([1, false, null]),
            json!({"\"雪": ["\\", 12]}),
        ] {
            let encoded = serde_json::to_vec(&value).unwrap();
            let mut size = RawSize(encoded.len());
            serde_json::to_writer(&mut size, &value).unwrap();
            assert_eq!(size.0, 0);
            assert!(serde_json::to_writer(&mut RawSize(encoded.len() - 1), &value).is_err());
        }
        let a: serde_json::Value = serde_json::from_str(r#"{ "text" : "\u96ea" }"#).unwrap();
        let b = json!({"text": "雪"});
        assert_eq!(a, b);
        assert_eq!(raw(Some(&a)).is_ok(), raw(Some(&b)).is_ok());
    }

    #[test]
    fn both_raw_fields_in_calls_and_updates_have_exact_byte_limits() {
        use serde_json::json;
        for field in ["rawInput", "rawOutput"] {
            for kind in ["tool_call", "tool_call_update"] {
                for text in [
                    "x".repeat(MAX_RAW_BYTES - 2),
                    format!("{}雪", "x".repeat(MAX_RAW_BYTES - 5)),
                    format!("{}\n", "x".repeat(MAX_RAW_BYTES - 4)),
                ] {
                    for over in [false, true] {
                        let value = json!(if over {
                            format!("{text}x")
                        } else {
                            text.clone()
                        });
                        assert_eq!(
                            serde_json::to_vec(&value).unwrap().len(),
                            MAX_RAW_BYTES + usize::from(over)
                        );
                        let mut update =
                            json!({"sessionUpdate": kind, "toolCallId": "call", "title": "title"});
                        update[field] = value;
                        let event: HarnessEvent =
                            serde_json::from_value(json!({"type": "update", "update": update}))
                                .unwrap();
                        assert_eq!(
                            accept(&mut events(), event).is_err(),
                            over,
                            "{kind} {field}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_permission_request_must_end_the_invocation() {
        let mut events = events();
        assert!(events.finish().is_err());
        accept(&mut events, permission("p", vec![option("allow")])).unwrap();
        assert!(events.finish().is_ok());
        assert!(
            accept(
                &mut events,
                HarnessEvent::Stop {
                    stop_reason: StopReason::EndTurn
                }
            )
            .is_err()
        );
    }

    #[test]
    fn a_stop_is_terminal() {
        let mut events = events();
        let stop = || HarnessEvent::Stop {
            stop_reason: StopReason::EndTurn,
        };
        accept(&mut events, stop()).unwrap();
        assert!(events.finish().is_ok());
        assert!(accept(&mut events, stop()).is_err());
    }

    #[test]
    fn cancelling_a_harness_requires_a_stop() {
        let operation = Operation::Harness {
            input: HarnessInput::Cancel,
        };
        let mut events = Events::new(&operation);
        assert!(events.finish().is_err());
        accept(&mut events, permission("p", vec![option("allow")])).unwrap();
        assert!(events.finish().is_err());
        let mut events = Events::new(&operation);
        accept(
            &mut events,
            HarnessEvent::Stop {
                stop_reason: StopReason::Cancelled,
            },
        )
        .unwrap();
        assert!(events.finish().is_ok());
    }

    #[test]
    fn permission_options_are_bounded_and_unique() {
        let mut events = events();
        assert!(
            accept(
                &mut events,
                permission("p", vec![option("allow"), option("allow")])
            )
            .is_err()
        );
        assert!(
            accept(
                &mut events,
                permission("p", (0..9).map(|i| option(&i.to_string())).collect())
            )
            .is_err()
        );
        accept(&mut events, permission("p", vec![option("allow")])).unwrap();
    }

    #[test]
    fn identifiers_and_tool_content_are_bounded() {
        let mut events = events();
        let empty = HarnessEvent::Update {
            update: SessionUpdate::ToolCall(ToolCall::new(String::new(), "title")),
        };
        assert!(accept(&mut events, empty).is_err());

        let mut events = self::events();
        let long = HarnessEvent::Update {
            update: SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block(
                "x".repeat(MAX_CHUNK + 1),
            ))),
        };
        assert!(accept(&mut events, long).is_err());
    }

    #[test]
    fn a_tool_update_cannot_carry_invalid_locations() {
        let mut events = events();
        let update = HarnessEvent::Update {
            update: SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .locations(vec![ToolCallLocation::new("/".repeat(MAX_PATH + 1))]),
            )),
        };
        assert!(accept(&mut events, update).is_err());
    }

    #[test]
    fn non_text_content_is_rejected_at_the_contract_boundary() {
        let mut events = events();
        let image: HarnessEvent = serde_json::from_value(serde_json::json!({
            "type": "update",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "image", "data": "AA==", "mimeType": "image/png"}
            }
        }))
        .unwrap();
        assert!(accept(&mut events, image).is_err());
    }

    #[test]
    fn session_update_kinds_outside_the_harness_contract_are_rejected() {
        let mut events = events();
        let update: HarnessEvent = serde_json::from_value(serde_json::json!({
            "type": "update",
            "update": {"sessionUpdate": "available_commands_update", "availableCommands": []}
        }))
        .unwrap();
        assert!(accept(&mut events, update).is_err());
    }

    /// ABI 8: a `Prompt` turn is a first-class operation, and the wire keeps
    /// refusing fields the contract does not name.
    #[test]
    fn harness_inputs_decode_the_contract_shape_and_reject_unknown_fields() {
        use serde_json::json;
        let mut events = Events::new(&Operation::Harness {
            input: HarnessInput::Prompt {
                prompt: vec![text_block("more")],
            },
        });
        accept(
            &mut events,
            HarnessEvent::Update {
                update: SessionUpdate::Plan(Plan::new(vec![])),
            },
        )
        .unwrap();
        accept(
            &mut events,
            HarnessEvent::Stop {
                stop_reason: StopReason::EndTurn,
            },
        )
        .unwrap();

        let start: HarnessInput =
            serde_json::from_value(json!({"type": "start", "prompt": [], "cwd": "/tmp/ene"}))
                .unwrap();
        assert_eq!(
            start,
            HarnessInput::Start {
                prompt: vec![],
                cwd: "/tmp/ene".into(),
            }
        );
        let prompt: HarnessInput =
            serde_json::from_value(json!({"type": "prompt", "prompt": []})).unwrap();
        assert_eq!(prompt, HarnessInput::Prompt { prompt: vec![] });
        for input in [
            json!({"type": "prompt", "prompt": [], "cwd": "/tmp"}),
            json!({"type": "start", "prompt": []}),
            json!({"type": "start", "prompt": [], "cwd": "/tmp", "bogus": 1}),
        ] {
            assert!(
                serde_json::from_value::<HarnessInput>(input.clone()).is_err(),
                "the input shape stays closed: {input}"
            );
        }
    }
}
