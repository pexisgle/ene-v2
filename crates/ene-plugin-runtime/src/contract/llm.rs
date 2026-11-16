//! LLM terminal event validation.
use super::Events;
use crate::{Error, Result};
use ene_plugin::Event;

impl Events {
    pub(super) fn accept_llm(&mut self, event: &Event) -> Result<()> {
        if let Event::Models { models } = event {
            if models.len() > 32768 {
                return Err(Error::Protocol);
            }
            self.terminal = true;
        }
        Ok(())
    }
}
