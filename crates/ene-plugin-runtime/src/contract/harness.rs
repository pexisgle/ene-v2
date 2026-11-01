//! ACP-shaped harness event validation.
use super::Events;
use crate::{Error, Result, limits::MAX_CHUNK};
use ene_plugin::{
    ContentBlock, HarnessEvent, PermissionOption, Plan, SessionUpdate, ToolCall, ToolCallContent,
    ToolCallUpdate,
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
    locations(call.locations.iter().map(|location| &location.path))?;
    raw(call.raw_input.as_ref())?;
    raw(call.raw_output.as_ref())
}

fn validate_tool_update(update: &ToolCallUpdate) -> Result<()> {
    identifier(&update.tool_call_id)?;
    if let Some(title) = &update.title {
        bounded(title, MAX_TITLE)?;
    }
    if let Some(name) = &update.name {
        bounded(name, MAX_IDENTIFIER)?;
    }
    if let Some(items) = &update.content {
        content(items)?;
    }
    if let Some(paths) = &update.locations {
        locations(paths.iter().map(|location| &location.path))?;
    }
    raw(update.raw_input.as_ref())?;
    raw(update.raw_output.as_ref())
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
    let mut ids = std::collections::BTreeSet::new();
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
        let ToolCallContent::Content { content } = item;
        text(content)?;
    }
    Ok(())
}

fn locations<'a>(paths: impl ExactSizeIterator<Item = &'a String>) -> Result<()> {
    if paths.len() > MAX_TOOL_LOCATIONS {
        return Err(Error::Protocol);
    }
    for path in paths {
        bounded(path, MAX_PATH)?;
    }
    Ok(())
}

fn text(block: &ContentBlock) -> Result<()> {
    let ContentBlock::Text(text) = block;
    bounded(&text.text, MAX_CHUNK)
}

fn raw(value: Option<&serde_json::Value>) -> Result<()> {
    if value.is_some_and(|value| {
        serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_RAW_BYTES)
    }) {
        return Err(Error::Protocol);
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
        ContentChunk, HarnessInput, Operation, PermissionOptionKind, StopReason, ToolCallLocation,
        ToolCallStatus,
    };

    fn events() -> Events {
        Events::new(&Operation::Harness {
            input: HarnessInput::Cancel,
        })
    }

    fn option(id: &str) -> PermissionOption {
        PermissionOption {
            option_id: id.into(),
            name: "Allow".into(),
            kind: PermissionOptionKind::AllowOnce,
        }
    }

    fn permission(request_id: &str, options: Vec<PermissionOption>) -> HarnessEvent {
        HarnessEvent::RequestPermission {
            request_id: request_id.into(),
            tool_call: ToolCallUpdate {
                tool_call_id: "call-1".into(),
                ..Default::default()
            },
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
            update: SessionUpdate::ToolCall(ToolCall {
                tool_call_id: String::new(),
                title: "title".into(),
                ..Default::default()
            }),
        };
        assert!(accept(&mut events, empty).is_err());

        let mut events = self::events();
        let long = HarnessEvent::Update {
            update: SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::text("x".repeat(MAX_CHUNK + 1)),
                message_id: None,
            }),
        };
        assert!(accept(&mut events, long).is_err());
    }

    #[test]
    fn a_tool_update_cannot_carry_invalid_locations() {
        let mut events = events();
        let update = HarnessEvent::Update {
            update: SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: "call-1".into(),
                status: Some(ToolCallStatus::InProgress),
                locations: Some(vec![ToolCallLocation {
                    path: "/".repeat(MAX_PATH + 1),
                    line: None,
                }]),
                ..Default::default()
            }),
        };
        assert!(accept(&mut events, update).is_err());
    }
}
