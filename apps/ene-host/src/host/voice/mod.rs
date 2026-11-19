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
pub(crate) type HttpResult<T> = std::result::Result<T, String>;

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
