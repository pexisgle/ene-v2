//! Checks extension semantics independently of the execution backend.
use crate::{
    Error, Result,
    limits::{MAX_CHUNK, MAX_STREAM_EVENTS},
};
use ene_plugin::*;
/// Bound guest service payloads and resource reads before invoking host services.
pub fn service_request(request: &ServiceRequest) -> Result<()> {
    if serde_json::to_vec(request)?.len() > MAX_CHUNK
        || matches!(request, ServiceRequest::Desktop {
            operation: DesktopOperation::ReadResource { length, .. }
        } if *length as usize > MAX_CHUNK)
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
pub struct Events {
    extension: Extension,
    terminal: bool,
    count: u64,
    speech: Option<(String, u64)>,
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
            terminal: false,
            count: 0,
            speech,
            audio_sequence: 0,
        }
    }
    pub fn accept(&mut self, event: &Event) -> Result<()> {
        if serde_json::to_vec(event)?.len() > MAX_CHUNK {
            return Err(Error::Protocol);
        }
        if self.terminal || self.count >= MAX_STREAM_EVENTS || event.extension() != self.extension {
            return Err(Error::Protocol);
        }
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
            Event::Audio { chunk } => {
                if self.speech.as_ref() != Some(&(chunk.utterance_id.clone(), chunk.generation))
                    || chunk.sequence != self.audio_sequence
                    || chunk.format != "pcm_s16le"
                    || !(8000..=192000).contains(&chunk.sample_rate)
                    || !(1..=8).contains(&chunk.channels)
                {
                    return Err(Error::Protocol);
                }
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&chunk.data_base64)
                    .map_err(|_| Error::Protocol)?;
                if bytes.len() > MAX_CHUNK || bytes.len() % (2 * chunk.channels as usize) != 0 {
                    return Err(Error::Protocol);
                }
                self.audio_sequence += 1;
                self.terminal = chunk.done;
            }
            Event::Harness {
                event:
                    HarnessEvent::Completed { .. }
                    | HarnessEvent::Failed { .. }
                    | HarnessEvent::Cancelled,
            } => self.terminal = true,
            _ => {}
        }
        self.count += 1;
        Ok(())
    }
    pub fn finish(&self) -> Result<()> {
        if matches!(self.extension, Extension::Llm | Extension::Tts) && !self.terminal {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
