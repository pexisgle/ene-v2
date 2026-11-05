//! Checks extension semantics independently of the execution backend.
mod character;
mod harness;
mod llm;

use crate::{Error, Result, limits::MAX_STREAM_EVENTS};
use ene_plugin::*;

pub struct Events {
    extension: Extension,
    models: bool,
    terminal: bool,
    harness_pending: bool,
    count: u64,
    character: Option<CharacterTarget>,
}

impl Events {
    pub fn new(operation: &Operation) -> Self {
        Self {
            extension: operation.extension(),
            models: matches!(operation, Operation::ListModels { .. }),
            terminal: false,
            harness_pending: false,
            count: 0,
            character: match operation {
                Operation::Character { target, .. } => Some(target.clone()),
                _ => None,
            },
        }
    }

    pub fn accept(&mut self, event: &Event) -> Result<()> {
        if self.terminal
            || self.harness_pending
            || self.count >= MAX_STREAM_EVENTS
            || event.extension() != self.extension
            || self.extension == Extension::Llm
                && matches!(event, Event::Models { .. }) != self.models
        {
            return Err(Error::Protocol);
        }
        match event {
            Event::Character { target, action } => self.accept_character(target, action)?,
            Event::Harness { event } => self.accept_harness(event)?,
            Event::Finished { .. } | Event::Models { .. } => self.accept_llm(event)?,
            _ => {}
        }
        self.count += 1;
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        if matches!(self.extension, Extension::Llm) && !self.terminal {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_specific_results_require_the_correct_terminal_event() {
        let generate = Operation::Generate {
            request: GenerationRequest {
                endpoint: String::new(),
                options: serde_json::json!({}),
                model: "fixture".into(),
                preamble: String::new(),
                messages: vec![],
                max_tokens: 1,
                tools: vec![],
            },
        };
        let list = Operation::ListModels {
            endpoint: String::new(),
            options: serde_json::json!({}),
        };
        for (operation, terminal, wrong) in [
            (
                generate,
                Event::Finished {
                    reason: "stop".into(),
                },
                Event::Models { models: vec![] },
            ),
            (
                list,
                Event::Models { models: vec![] },
                Event::Delta {
                    text: "wrong".into(),
                },
            ),
        ] {
            let mut events = Events::new(&operation);
            assert!(events.finish().is_err());
            assert!(events.accept(&wrong).is_err());
            events.accept(&terminal).unwrap();
            events.finish().unwrap();
            assert!(events.accept(&terminal).is_err());
        }
    }

    #[test]
    fn character_action_limits_are_enforced_by_the_runtime() {
        let target = CharacterTarget {
            device_id: "device".into(),
            character_id: "character".into(),
        };
        let mut events = Events::new(&Operation::Character {
            target: target.clone(),
            event: CharacterEvent::Activated,
        });
        for action in [
            CharacterAction::Speak {
                text: "a".repeat(16385),
            },
            CharacterAction::Expression {
                name: "a".repeat(129),
                duration_ms: 1,
            },
            CharacterAction::Motion {
                asset: "a".repeat(129),
                duration_ms: 1,
            },
            CharacterAction::Expression {
                name: "happy".into(),
                duration_ms: 60001,
            },
            CharacterAction::Motion {
                asset: "wave".into(),
                duration_ms: 60001,
            },
        ] {
            assert!(
                events
                    .accept(&Event::Character {
                        target: target.clone(),
                        action
                    })
                    .is_err()
            );
        }
        events
            .accept(&Event::Character {
                target,
                action: CharacterAction::Expression {
                    name: "a".repeat(128),
                    duration_ms: 60000,
                },
            })
            .unwrap();
    }

    #[test]
    fn character_events_cannot_change_the_authorized_target() {
        let target = CharacterTarget {
            device_id: "device".into(),
            character_id: "character".into(),
        };
        let mut events = Events::new(&Operation::Character {
            target: target.clone(),
            event: CharacterEvent::Activated,
        });
        let event = |target| Event::Character {
            target,
            action: CharacterAction::Speak {
                text: "hello".into(),
            },
        };
        for other in [
            CharacterTarget {
                device_id: "other".into(),
                ..target.clone()
            },
            CharacterTarget {
                character_id: "other".into(),
                ..target.clone()
            },
        ] {
            assert!(events.accept(&event(other)).is_err());
        }
        events.accept(&event(target)).unwrap();
    }
}
