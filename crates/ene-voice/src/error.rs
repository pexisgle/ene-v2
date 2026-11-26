//! Voice pipeline error and future types.

use std::{future::Future, pin::Pin};

/// Heap-allocated `Send` future returned by TTS and STT engines.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result alias for TTS/STT engine transport and audio processing operations.
pub type HttpResult<T> = std::result::Result<T, VoiceHttpError>;

/// Errors produced by HTTP/plugin TTS/STT synthesis, transcription, and PCM/WAV framing.
#[derive(Debug, thiserror::Error)]
pub enum VoiceHttpError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Decode(String),
    #[error("{0}")]
    LimitExceeded(String),
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
