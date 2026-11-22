//! Host-side ECS adapter over the `ene-voice` pipeline (`docs/design/voice-avatar.md` §4–§5).

mod engine;
mod plugin;
mod session;
#[cfg(test)]
mod tests;

pub(crate) use ene_voice::{
    AudioSubscription, BoxFuture, HttpEngine, HttpResult, InputBinding, MAX_PROBE_CHARS,
    MAX_STT_BYTES, MIN_STT_BYTES, MIN_STT_RMS, SAMPLE_RATE, STT_SAMPLE_RATES, STT_SILENCE_WINDOW,
    STT_TIMEOUT, SetupError, SttEngine, TtsEngine, VOICE_UNAVAILABLE, VoiceHttpError,
    parse_binding, speech, synthesize_for_probe, voicevox,
};
#[cfg(test)]
pub(crate) use engine::Engines;
pub(crate) use engine::{companion_tts_source, stt_source, tts_source};
pub(crate) use session::{
    VoicePlugin, accept_transcription, bind_input, flush, is_still_active, on_client_activated,
    on_client_moved, on_stream_text, on_turn_end, play_once, subscribe_audio, unsubscribe_audio,
};

/// Bevy ECS resource wrapper over [`ene_voice::VoicePipeline`].
#[derive(Clone, Debug, bevy_ecs::prelude::Resource)]
pub(crate) struct Voice(ene_voice::VoicePipeline);

impl Voice {
    pub(crate) fn new(runtime: tokio::runtime::Handle) -> Self {
        Self(ene_voice::VoicePipeline::new(runtime))
    }
}

impl std::ops::Deref for Voice {
    type Target = ene_voice::VoicePipeline;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
