//! Voice pipeline: PCM/WAV framing, spoken-text preparation, VOICEVOX lifecycle,
//! HTTP TTS/STT engines, and the ECS-free speech floor & sentence synthesis pipeline.

pub mod engine;
mod engine_config;
mod engine_http;
pub mod error;
pub mod pcm;
pub mod protocol;
pub mod session;
pub mod speech;
pub mod voicevox;

pub use engine::{
    HttpEngine, SttEngine, TtsEngine, parse_binding, select_connection, select_connection_by_id,
};
pub use error::{BoxFuture, HttpResult, SetupError, VoiceHttpError, VoiceSessionError};
pub use pcm::{SAMPLE_RATE, STT_SAMPLE_RATES, parse_wav, resample_mono, wav_wrap, wire_pcm};
pub use protocol::{
    AudioChunk, FlushControl, FlushReason, UtteranceFinished, VoiceBinding, VoiceOutputEvent,
};
pub use session::{
    AudioSubscription, Dispatch, EndedUtterance, InputBinding, MAX_PROBE_CHARS, MAX_SENTENCE_CHARS,
    MAX_STT_BYTES, MIN_STT_BYTES, MIN_STT_RMS, STT_SILENCE_WINDOW, STT_TIMEOUT, VOICE_UNAVAILABLE,
    VoicePipeline, synthesize, synthesize_for_probe,
};
pub use voicevox::{VoicevoxError, VoicevoxManager};
