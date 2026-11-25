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

/// Text plus the voice and format a provider should render.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisRequest {
    /// Provider endpoint from the connection; the host overwrites it.
    pub endpoint: String,
    /// Adapter options from the connection; the host overwrites them.
    pub options: Value,
    /// Voice model to call.
    pub model: String,
    /// The text to speak.
    pub text: String,
    /// Provider voice id.
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
    /// Handle the host reads the audio back through.
    pub handle: String,
    /// Sample rate of the returned audio, in Hz.
    pub sample_rate: u32,
    /// Sample layout of the returned audio.
    pub format: AudioFormat,
    /// The whole audio, so the host can bound it before reading.
    pub bytes: u64,
}

/// Audio to turn into text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionRequest {
    /// Provider endpoint from the connection; the host overwrites it.
    pub endpoint: String,
    /// Adapter options from the connection; the host overwrites them.
    pub options: Value,
    /// The transcription model to call.
    pub model: String,
    /// Signed 16-bit little-endian mono samples.
    pub pcm_base64: String,
    /// Sample rate of `pcm_base64`, in Hz.
    pub sample_rate: u32,
    /// Language hint, when the host has one.
    pub language: Option<String>,
}

/// The text a provider recognised.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionResult {
    /// The recognised text.
    pub text: String,
}

/// What a speech provider supports beyond the base contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechCapabilities {
    /// Whether the provider accepts a speed multiplier.
    pub speed: bool,
    /// Whether it accepts a language hint.
    pub language: bool,
}
