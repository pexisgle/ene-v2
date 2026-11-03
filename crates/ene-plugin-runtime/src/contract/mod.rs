//! Checks extension semantics independently of the execution backend.
mod character;
mod harness;
mod llm;
mod speech;

use crate::{Error, Result, limits::MAX_STREAM_EVENTS};
use ene_plugin::*;

pub struct Events {
    extension: Extension,
    models: bool,
    terminal: bool,
    harness_pending: bool,
    count: u64,
    speech: Option<(String, u64)>,
    character: Option<CharacterTarget>,
    audio_sequence: u64,
}

impl Events {
    pub fn new(operation: &Operation) -> Self {
        let speech = match operation {
            Operation::Speech {
                input:
                    SpeechInput::Synthesize {
                        utterance_id,
                        generation,
                        ..
                    },
            } => Some((utterance_id.clone(), *generation)),
            Operation::Speech {
                input:
                    SpeechInput::Transcribe {
                        utterance_id,
                        audio,
                    },
            } => Some((utterance_id.clone(), audio.generation)),
            _ => None,
        };
        Self {
            extension: operation.extension(),
            models: matches!(operation, Operation::ListModels { .. }),
            terminal: false,
            harness_pending: false,
            count: 0,
            speech,
            character: match operation {
                Operation::Character { target, .. } => Some(target.clone()),
                _ => None,
            },
            audio_sequence: 0,
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
            Event::Audio { chunk } => self.accept_audio(chunk)?,
            Event::Harness { event } => self.accept_harness(event)?,
            Event::Finished { .. } | Event::Models { .. } => self.accept_llm(event)?,
            Event::Transcript { .. } => self.terminal = true,
            _ => {}
        }
        self.count += 1;
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        if matches!(
            self.extension,
            Extension::Llm | Extension::Tts | Extension::Stt
        ) && !self.terminal
        {
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
        let transcribe = Operation::Speech {
            input: SpeechInput::Transcribe {
                utterance_id: "u".into(),
                audio: AudioChunk {
                    utterance_id: "u".into(),
                    generation: 1,
                    sequence: 0,
                    format: "pcm_s16le".into(),
                    sample_rate: 16000,
                    channels: 1,
                    data_base64: "AAA=".into(),
                    done: true,
                },
            },
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
            (
                transcribe,
                Event::Transcript {
                    text: String::new(),
                },
                Event::Finished {
                    reason: "stop".into(),
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

    #[test]
    fn speech_generations_sequences_and_terminal_state_are_checked() {
        let operation = Operation::Speech {
            input: SpeechInput::Synthesize {
                utterance_id: "u".into(),
                generation: 4,
                text: "hello".into(),
                voice: "v".into(),
            },
        };
        let mut events = Events::new(&operation);
        let mut chunk = AudioChunk {
            utterance_id: "u".into(),
            generation: 3,
            sequence: 0,
            format: "pcm_s16le".into(),
            sample_rate: 16000,
            channels: 1,
            data_base64: "AAA=".into(),
            done: true,
        };
        assert!(
            events
                .accept(&Event::Audio {
                    chunk: chunk.clone()
                })
                .is_err()
        );
        chunk.generation = 4;
        chunk.sequence = 1;
        assert!(
            events
                .accept(&Event::Audio {
                    chunk: chunk.clone()
                })
                .is_err()
        );
        chunk.sequence = 0;
        events
            .accept(&Event::Audio {
                chunk: chunk.clone(),
            })
            .unwrap();
        events.finish().unwrap();
        assert!(events.accept(&Event::Audio { chunk }).is_err());
    }
}
