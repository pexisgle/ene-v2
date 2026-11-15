//! Speech contract: synthesis and transcription.
//!
//! Audio does not travel in one reply. A synthesis answer is a handle the host
//! reads back in chunks ([`crate::ServiceRequest::AudioRead`]), because a
//! minute of speech is larger than the message bound a single reply has.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The sample layout a provider returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// Signed 16-bit little-endian mono samples.
    Pcm16,
    /// A RIFF/WAVE container.
    Wav,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub text: String,
    pub voice: String,
    /// Zero keeps the provider's own rate.
    pub speed: f32,
    /// What the host can play. A provider that returns something else
    /// converts it or fails.
    pub format: AudioFormat,
}

/// The handle a synthesis answer opens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisResult {
    pub handle: String,
    pub sample_rate: u32,
    pub format: AudioFormat,
    /// The whole audio, so the host can bound it before reading.
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    /// Signed 16-bit little-endian mono samples.
    pub pcm_base64: String,
    pub sample_rate: u32,
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionResult {
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechCapabilities {
    /// Whether the provider accepts a speed multiplier.
    pub speed: bool,
    /// Whether it accepts a language hint.
    pub language: bool,
}
