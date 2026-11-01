//! Character action validation.
use super::Events;
use crate::{Error, Result};
use ene_plugin::{CharacterAction, CharacterTarget};

impl Events {
    pub(super) fn accept_character(
        &mut self,
        target: &CharacterTarget,
        action: &CharacterAction,
    ) -> Result<()> {
        if self.character.as_ref() != Some(target) {
            return Err(Error::Protocol);
        }
        match action {
            CharacterAction::Speak { text } if text.len() > 16384 => Err(Error::Protocol),
            CharacterAction::Expression { name, duration_ms }
            | CharacterAction::Motion {
                asset: name,
                duration_ms,
            } if name.len() > 128 || *duration_ms > 60000 => Err(Error::Protocol),
            _ => Ok(()),
        }
    }
}
