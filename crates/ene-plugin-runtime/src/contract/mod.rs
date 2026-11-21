//! Checks extension semantics independently of the execution backend.
mod character;
mod harness;
mod llm;

use crate::{
    Error, Result,
    limits::{MAX_HTTP_HEADERS, MAX_STREAM_EVENTS},
};
use ene_plugin::*;

#[derive(Debug)]
pub struct Events {
    extension: Extension,
    models: bool,
    raw: bool,
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
            raw: matches!(operation, Operation::GenerateHttp { .. }),
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
        if self.raw {
            match event {
                Event::Http { chunk } => {
                    if chunk.handle.len() > 1024
                        || chunk.headers.len() > 64
                        || chunk
                            .headers
                            .iter()
                            .map(|(key, value)| key.len().saturating_add(value.len()))
                            .sum::<usize>()
                            > MAX_HTTP_HEADERS
                        || chunk.data_base64.len() > crate::limits::MAX_CHUNK
                    {
                        return Err(Error::Protocol);
                    }
                    if chunk.done {
                        self.terminal = true;
                    }
                    self.count += 1;
                    return Ok(());
                }
                _ => return Err(Error::Protocol),
            }
        }
        if matches!(event, Event::Http { .. }) {
            return Err(Error::Protocol);
        }
        match event {
            Event::Character { target, action } => self.accept_character(target, action)?,
            Event::Harness { event } => self.accept_harness(event)?,
            Event::Models { .. } => self.accept_llm(event)?,
            Event::Audio {
                data_base64,
                sample_rate,
                done,
            } => {
                if data_base64.len() > crate::limits::MAX_CHUNK || *sample_rate > 192_000 {
                    return Err(Error::Protocol);
                }
                if *done {
                    self.terminal = true;
                }
            }
            Event::Transcription { text, is_final } => {
                if text.len() > 16_384 {
                    return Err(Error::Protocol);
                }
                if *is_final {
                    self.terminal = true;
                }
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
        let list = Operation::ListModels {
            endpoint: String::new(),
            options: serde_json::json!({}),
        };
        let mut events = Events::new(&list);
        assert!(events.finish().is_err());
        assert!(
            events
                .accept(&Event::Http {
                    chunk: ene_plugin::HttpChunk {
                        handle: String::new(),
                        status: 200,
                        headers: Default::default(),
                        data_base64: String::new(),
                        done: true,
                    }
                })
                .is_err()
        );
        events.accept(&Event::Models { models: vec![] }).unwrap();
        events.finish().unwrap();
        assert!(events.accept(&Event::Models { models: vec![] }).is_err());
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
    fn f22_streaming_audio_and_transcription_events_validate_bounds_and_terminal() {
        let mut tts = Events::new(&Operation::Synthesize {
            request: SynthesisRequest {
                endpoint: "http://127.0.0.1".into(),
                options: serde_json::json!({}),
                model: "m".into(),
                text: "hi".into(),
                voice: "v".into(),
                speed: 1.0,
                format: AudioFormat::Pcm16,
            },
        });
        tts.accept(&Event::Audio {
            data_base64: "AAAA".into(),
            sample_rate: 24_000,
            done: false,
        })
        .unwrap();
        tts.accept(&Event::Audio {
            data_base64: String::new(),
            sample_rate: 24_000,
            done: true,
        })
        .unwrap();
        // Events after `done: true` must be rejected.
        assert!(
            tts.accept(&Event::Audio {
                data_base64: "AAAA".into(),
                sample_rate: 24_000,
                done: false,
            })
            .is_err()
        );

        let mut stt = Events::new(&Operation::Transcribe {
            request: TranscriptionRequest {
                endpoint: "http://127.0.0.1".into(),
                options: serde_json::json!({}),
                model: "m".into(),
                pcm_base64: "AAAA".into(),
                sample_rate: 16_000,
                language: None,
            },
        });
        stt.accept(&Event::Transcription {
            text: "こんに".into(),
            is_final: false,
        })
        .unwrap();
        stt.accept(&Event::Transcription {
            text: "こんにちは".into(),
            is_final: true,
        })
        .unwrap();
        assert!(
            stt.accept(&Event::Transcription {
                text: "extra".into(),
                is_final: false,
            })
            .is_err()
        );
    }
}
