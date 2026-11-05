//! ACP-shaped harness event validation.
use super::Events;
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

impl Events {
    pub(super) fn accept_harness(&mut self, event: &HarnessEvent) -> Result<()> {
        match event {
            HarnessEvent::Update { update } => validate_update(update),
            HarnessEvent::RequestPermission {
                request_id,
                tool_call,
                options,
            } => {
                identifier(request_id)?;
                validate_tool_update(tool_call)?;
                validate_options(options)?;
                // The host answers through a separate invocation; nothing can
                // follow the request inside this one.
                self.harness_pending = true;
                Ok(())
            }
            HarnessEvent::Stop { .. } => {
                self.terminal = true;
                Ok(())
            }
        }
    }
}

fn validate_update(update: &SessionUpdate) -> Result<()> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(message) = &chunk.message_id {
                identifier(message)?;
            }
            text(&chunk.content)
        }
        SessionUpdate::ToolCall(call) => validate_tool_call(call),
        SessionUpdate::ToolCallUpdate(update) => validate_tool_update(update),
        SessionUpdate::Plan(plan) => validate_plan(plan),
    }
}

fn validate_tool_call(call: &ToolCall) -> Result<()> {
    identifier(&call.tool_call_id)?;
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
    identifier(&update.tool_call_id)?;
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
        identifier(&option.option_id)?;
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

fn raw(value: Option<&serde_json::Value>) -> Result<()> {
    if value.is_some_and(|value| {
        serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_RAW_BYTES)
    }) {
        return Err(Error::Protocol);
    }
    Ok(())
}

fn identifier(value: &impl std::fmt::Display) -> Result<()> {
    let value = value.to_string();
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
            input: HarnessInput::Cancel,
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
    fn a_permission_request_must_end_the_invocation() {
        let mut events = events();
        accept(&mut events, permission("p", vec![option("allow")])).unwrap();
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
        assert!(accept(&mut events, stop()).is_err());
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
}
