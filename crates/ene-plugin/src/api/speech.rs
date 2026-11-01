//! Speech synthesis (TTS) and transcription (STT) payloads.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpeechInput {
    Synthesize {
        utterance_id: String,
        #[serde(default)]
        generation: u64,
        text: String,
        voice: String,
    },
    Transcribe {
        utterance_id: String,
        audio: AudioChunk,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioChunk {
    pub utterance_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub format: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub data_base64: String,
    pub done: bool,
}
