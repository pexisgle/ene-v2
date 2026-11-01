//! LLM terminal event validation.
use super::Events;
use crate::{Error, Result};
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
            _ => {}
        }
        Ok(())
    }
}
