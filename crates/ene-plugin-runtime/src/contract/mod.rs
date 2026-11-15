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
    /// Tool calls this reply has asked for.
    calls: usize,
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
            calls: 0,
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
            // Every LLM event goes through its own check, not only the
            // terminal ones: a tool call the host cannot answer has to be
            // refused where it arrives.
            Event::Finished { .. } | Event::Models { .. } | Event::ToolCall { .. } => {
                self.accept_llm(event)?;
            }
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
                tools: vec![],
                max_tokens: 1,
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

#[cfg(test)]
mod tool_tests {
    use super::*;
    use crate::limits::MAX_TOOL_CALLS;

    fn generate() -> Operation {
        Operation::Generate {
            request: GenerationRequest {
                endpoint: String::new(),
                options: serde_json::json!({}),
                model: "fixture".into(),
                preamble: String::new(),
                messages: vec![],
                tools: vec![],
                max_tokens: 1,
            },
        }
    }

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> Event {
        Event::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    #[test]
    fn a_usable_tool_call_is_accepted_and_the_turn_still_ends() {
        let mut events = Events::new(&generate());
        events
            .accept(&call("call-1", "lookup", serde_json::json!({"q": "x"})))
            .unwrap();
        // A call is not terminal: the reply still has to say it finished.
        assert!(events.finish().is_err());
        events
            .accept(&Event::Finished {
                reason: "tool_calls".into(),
            })
            .unwrap();
        events.finish().unwrap();
    }

    #[test]
    fn a_call_the_host_could_not_answer_is_refused() {
        for bad in [
            call("", "lookup", serde_json::json!({})),
            call("call-1", "", serde_json::json!({})),
            // The arguments must be an object, so a host can hand them to a
            // tool without reinterpreting them.
            call("call-1", "lookup", serde_json::json!("not an object")),
            call("call-1", "lookup", serde_json::json!([1, 2])),
        ] {
            let mut events = Events::new(&generate());
            assert!(events.accept(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn one_reply_may_not_ask_for_more_calls_than_the_host_answers() {
        let mut events = Events::new(&generate());
        for index in 0..MAX_TOOL_CALLS {
            events
                .accept(&call(
                    &format!("call-{index}"),
                    "lookup",
                    serde_json::json!({}),
                ))
                .unwrap();
        }
        assert!(
            events
                .accept(&call("one-too-many", "lookup", serde_json::json!({})))
                .is_err()
        );
    }
}
