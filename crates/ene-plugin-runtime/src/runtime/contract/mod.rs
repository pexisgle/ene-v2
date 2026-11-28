//! Checks extension semantics independently of the execution backend.
mod harness;

use crate::runtime::{Result, limits::MAX_STREAM_EVENTS};
use ene_plugin::*;

#[derive(Debug, Default, PartialEq, Eq)]
enum StreamState {
    #[default]
    Streaming,
    AwaitingPermission,
    Finished,
}

#[derive(Debug)]
pub struct Events {
    extension: Extension,
    models: bool,
    raw: bool,
    cancelling_harness: bool,
    state: StreamState,
    count: u64,
    character: Option<CharacterTarget>,
}

impl Events {
    pub fn new(operation: &Operation) -> Self {
        Self {
            extension: operation.extension(),
            models: matches!(operation, Operation::ListModels { .. }),
            raw: matches!(operation, Operation::GenerateHttp { .. }),
            cancelling_harness: matches!(
                operation,
                Operation::Harness {
                    input: HarnessInput::Cancel
                }
            ),
            state: StreamState::Streaming,
            count: 0,
            character: match operation {
                Operation::Character { target, .. } => Some(target.clone()),
                _ => None,
            },
        }
    }

    pub fn accept(&mut self, event: &Event) -> Result<()> {
        if self.state != StreamState::Streaming
            || self.count >= MAX_STREAM_EVENTS
            || event.extension() != self.extension
            || self.extension == Extension::Llm
                && matches!(event, Event::Models { .. }) != self.models
        {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        if self.raw {
            match event {
                Event::Http { chunk } => {
                    if chunk.handle.len() > 1024
                        || chunk.headers.len() > MAX_HTTP_HEADER_FIELDS
                        || chunk
                            .headers
                            .iter()
                            .map(|(key, value)| key.len().saturating_add(value.len()))
                            .sum::<usize>()
                            > MAX_HTTP_HEADER_BYTES
                        || chunk.data_base64.len() > crate::runtime::limits::MAX_CHUNK
                    {
                        return Err(anyhow::anyhow!("invalid_plugin_contract"));
                    }
                    if chunk.done {
                        self.state = StreamState::Finished;
                    }
                    self.count += 1;
                    return Ok(());
                }
                _ => return Err(anyhow::anyhow!("invalid_plugin_contract")),
            }
        }
        if matches!(event, Event::Http { .. }) {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        match event {
            Event::Character { target, action } => self.accept_character(target, action)?,
            Event::Harness { event } => match harness::validate(event)? {
                harness::Progress::Update => {}
                harness::Progress::AwaitingPermission => {
                    self.state = StreamState::AwaitingPermission
                }
                harness::Progress::Stopped => self.state = StreamState::Finished,
            },
            Event::Models { models } => {
                if models.len() > 32_768 {
                    return Err(anyhow::anyhow!("invalid_plugin_contract"));
                }
                self.state = StreamState::Finished;
            }
            Event::Audio {
                data_base64,
                sample_rate,
                done,
            } => {
                if data_base64.len() > crate::runtime::limits::MAX_CHUNK || *sample_rate > 192_000 {
                    return Err(anyhow::anyhow!("invalid_plugin_contract"));
                }
                if *done {
                    self.state = StreamState::Finished;
                }
            }
            Event::Transcription { text, is_final } => {
                if text.len() > 16_384 {
                    return Err(anyhow::anyhow!("invalid_plugin_contract"));
                }
                if *is_final {
                    self.state = StreamState::Finished;
                }
            }
            _ => {}
        }
        self.count += 1;
        Ok(())
    }

    fn accept_character(&self, target: &CharacterTarget, action: &CharacterAction) -> Result<()> {
        if self.character.as_ref() != Some(target) {
            return Err(anyhow::anyhow!("invalid_plugin_contract"));
        }
        match action {
            CharacterAction::Speak { text } if text.len() > 16_384 => {
                Err(anyhow::anyhow!("invalid_plugin_contract"))
            }
            CharacterAction::Expression { name, duration_ms }
            | CharacterAction::Motion {
                asset: name,
                duration_ms,
            } if name.len() > 128 || *duration_ms > 60_000 => {
                Err(anyhow::anyhow!("invalid_plugin_contract"))
            }
            _ => Ok(()),
        }
    }

    pub fn finish(&self) -> Result<()> {
        let complete = match self.extension {
            Extension::Llm => self.state == StreamState::Finished,
            Extension::Harness if self.cancelling_harness => self.state == StreamState::Finished,
            Extension::Harness => self.state != StreamState::Streaming,
            _ => true,
        };
        if complete {
            Ok(())
        } else {
            Err(anyhow::anyhow!("invalid_plugin_contract"))
        }
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
