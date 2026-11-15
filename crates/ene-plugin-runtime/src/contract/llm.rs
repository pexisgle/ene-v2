//! LLM terminal event validation.
use super::Events;
use crate::{Error, Result, limits::MAX_TOOL_CALLS};
use ene_plugin::Event;

impl Events {
    pub(super) fn accept_llm(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::Finished { reason } => {
                if reason.is_empty() || reason.len() > 128 {
                    return Err(Error::Protocol);
                }
                self.terminal = true;
            }
            Event::Models { models } => {
                if models.len() > 32768 {
                    return Err(Error::Protocol);
                }
                self.terminal = true;
            }
            Event::ToolCall {
                id,
                name,
                arguments,
            } => {
                // A call the host cannot answer is a call that hangs the
                // turn, so an unusable one is refused here rather than
                // handed on.
                if id.is_empty()
                    || id.len() > 256
                    || name.is_empty()
                    || name.len() > 256
                    || !arguments.is_object()
                    || self.calls >= MAX_TOOL_CALLS
                {
                    return Err(Error::Protocol);
                }
                self.calls += 1;
            }
            _ => {}
        }
        Ok(())
    }
}
