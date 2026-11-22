//! Voice pipeline error and future types.

use std::{future::Future, pin::Pin};

/// Heap-allocated `Send` future returned by TTS and STT engines.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result alias for TTS/STT engine transport and audio processing operations.
pub type HttpResult<T> = std::result::Result<T, VoiceHttpError>;

/// Errors produced by HTTP/plugin TTS/STT synthesis, transcription, and PCM/WAV framing.
#[derive(Debug, thiserror::Error)]
pub enum VoiceHttpError {
    #[error("http client: {0}")]
    ClientInit(String),
    #[error("the {label} response failed: {error}")]
    ResponseRead { label: String, error: String },
    #[error("the {0} response exceeded the host limit")]
    ResponseTooLarge(String),
    #[error("the audio has an odd number of PCM bytes")]
    OddInputPcmBytes,
    #[error("the TTS engine returned an odd number of PCM bytes")]
    OddOutputPcmBytes,
    #[error("the audio is too large for a WAV")]
    WavTooLarge,
    #[error("the audio sample rate or length cannot be written to a WAV")]
    WavInvalidParams,
    #[error("the TTS endpoint did not return a valid WAV")]
    InvalidWav,
    #[error("the TTS endpoint returned an unsupported WAV format")]
    UnsupportedWavFormat,
    #[error("the TTS endpoint returned a WAV without audio data")]
    MissingWavData,
    #[error("the TTS engine returned audio without a sample rate")]
    MissingSampleRate,
    #[error("the TTS sample rate cannot be converted: {0}")]
    Resample(String),
    #[error("text is required")]
    TextRequired,
    #[error("text is limited to {0} characters")]
    TextTooLong(usize),
    #[error("the TTS connection timed out")]
    TtsTimeout,
    #[error("the speech provider returned more audio than the host reads")]
    PluginAudioTooLarge,
    #[error("the speech provider's audio: {0}")]
    PluginAudioRead(String),
    #[error("the speech provider returned undecodable audio")]
    PluginAudioDecode,
    #[error("the speech provider emitted undecodable audio chunk")]
    PluginStreamDecode,
    #[error("the speech provider exceeded the maximum streamed audio size")]
    PluginStreamTooLarge,
    #[error("the speech provider: {0}")]
    PluginCall(String),
    #[error("the speech stream collector stopped")]
    PluginCollectorStopped,
    #[error("the speech provider returned no audio handle")]
    PluginMissingHandle,
    #[error("the TTS endpoint answered a non-JSON audio query")]
    InvalidAudioQuery,
    #[error("the STT multipart body: {0}")]
    SttMultipart(String),
    #[error("the STT endpoint answered a non-JSON transcript")]
    InvalidSttJson,
    #[error("the STT endpoint answered a non-UTF-8 transcript")]
    InvalidSttUtf8,
    #[error("the TTS engine received an STT protocol")]
    TtsProtocolMismatch,
    #[error("the STT engine received a TTS protocol")]
    SttProtocolMismatch,
    #[error("the {label} endpoint is unreachable: {error}")]
    EndpointUnreachable { label: String, error: String },
    #[error("the {label} endpoint answered {status}")]
    EndpointStatus { label: String, status: u16 },
    #[error("{0}")]
    Engine(String),
}

/// Errors encountered when resolving or constructing a TTS/STT engine.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    #[error("voice requires a configured TTS/STT connection")]
    Missing,
    #[error("{0}")]
    Disabled(String),
    #[error("{0}")]
    Broken(String),
    #[error("{0}")]
    Unsupported(String),
}

/// Errors returned by [`crate::VoicePipeline`] playback and floor operations.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VoiceSessionError {
    #[error("no client is subscribed to this companion's audio")]
    NoSubscriber,
    #[error("the audio subscription is closed")]
    StreamMissing,
    #[error("the audio queue is full")]
    AudioQueueFull,
    #[error("the audio subscription is closed")]
    StreamClosed,
    #[error("no client owns this companion's audio output")]
    NoOwner,
}
