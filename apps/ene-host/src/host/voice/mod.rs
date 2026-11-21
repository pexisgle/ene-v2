//! Host-side voice pipeline: recognition input, sentence-level synthesis
//! output and the speech floor (docs/design/voice-avatar.md §4–§5,
//! docs/api.md §6–§7). Recognition runs in the Connect layer; synthesis tasks
//! run on the host runtime and release audio in `sequence_no` order.
//!
//! A turn streams text sentence by sentence while the host synthesizes up to
//! 3 sentences ahead. The host releases the speech floor when the turn ends and
//! emits `UtteranceFinished` only once every sentence queued for the utterance
//! has resolved, so the event stays the last audio event of the utterance. A
//! sentence whose synthesis fails adds no audio and nothing waits for it.
//! `generation_id` and `FlushControl` drop the chunks of an older generation
//! after a barge-in; `sequence_no` only carries the enqueue order.
mod engine;
mod pcm;
mod plugin;
mod session;
pub(crate) mod speech;
#[cfg(test)]
mod tests;
pub(crate) mod voicevox;

use std::{future::Future, pin::Pin};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum VoiceHttpError {
    #[error(transparent)]
    Net(#[from] crate::host::net::NetError),
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
    #[cfg(test)]
    #[error("{0}")]
    Mock(String),
}

#[cfg(test)]
impl From<&'static str> for VoiceHttpError {
    fn from(message: &'static str) -> Self {
        Self::Mock(message.to_owned())
    }
}

pub(crate) type HttpResult<T> = std::result::Result<T, VoiceHttpError>;

#[cfg(test)]
pub(crate) use engine::Engines;
pub(crate) use engine::{
    SetupError, SttEngine, TtsEngine, companion_tts_source, parse_binding, stt_source, tts_source,
};
pub(crate) use pcm::{SAMPLE_RATE, STT_SAMPLE_RATES};
pub(crate) use session::{
    AudioSubscription, InputBinding, MAX_PROBE_CHARS, MAX_STT_BYTES, MIN_STT_BYTES, MIN_STT_RMS,
    STT_SILENCE_WINDOW, STT_TIMEOUT, VOICE_UNAVAILABLE, VoicePlugin, accept_transcription,
    bind_input, flush, is_still_active, on_client_activated, on_client_moved, on_stream_text,
    on_turn_end, play_once, subscribe_audio, synthesize_for_probe, unsubscribe_audio,
};

#[cfg(test)]
use {
    crate::host::{
        db::Store,
        error::HostError,
        model::Credentials,
        notify::Notifier,
        settings::{Connection, ConnectionConfig, ConnectionKind, Endpoint},
    },
    bevy_ecs::prelude::World,
    ene_rpc as w,
    engine::{HttpEngine, voice_connection},
    pcm::{resample_mono, wav_wrap, wire_pcm},
    session::{
        Dispatch, MAX_SENTENCE_CHARS, Owner, Voice, audio_event, deliver, flush_sentences,
        split_sentences,
    },
    std::{sync::Arc, time::Duration},
    tokio::sync::Notify,
};
