//! Constants, output event builders, sentence splitting, and synthesis helpers.

use crate::{
    engine::TtsEngine,
    error::{HttpResult, VoiceHttpError},
    pcm::SAMPLE_RATE,
    protocol::{AudioChunk, FlushControl, FlushReason, UtteranceFinished, VoiceOutputEvent},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(20);
pub const STT_TIMEOUT: Duration = Duration::from_secs(30);
pub const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_SENTENCE_CHARS: usize = 240;
/// The longest text a settings probe may ask the engine to speak.
pub const MAX_PROBE_CHARS: usize = MAX_SENTENCE_CHARS;
pub const STT_SILENCE_WINDOW: Duration = Duration::from_millis(700);
/// Below this the recognizer is answering noise. Whisper-family models emit a
/// stock phrase ("ご視聴ありがとうございました" and its English equivalents) on
/// silence, so a buffer that is short or nearly flat never reaches the engine.
pub const MIN_STT_BYTES: usize = 4_800 * 2;
/// The normalized RMS floor of a short window worth sending to STT.
pub const MIN_STT_RMS: f32 = 0.004;
pub const MAX_STT_BYTES: usize = 48_000 * 20;
pub const VOICE_UNAVAILABLE: &str = "voice requires a configured TTS/STT connection";

#[derive(Debug)]
pub struct AudioSubscription {
    pub subscription_id: String,
    pub receiver: mpsc::Receiver<VoiceOutputEvent>,
    pub control: mpsc::UnboundedReceiver<VoiceOutputEvent>,
}

/// A finished utterance the host relays to the peers (voice-avatar.md §5.3).
#[derive(Clone, Debug)]
pub struct EndedUtterance {
    pub client_id: String,
    pub utterance_id: String,
    pub generation_id: u64,
    pub total_sentences: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dispatch {
    Sent,
    Stale,
    Full,
    Closed,
}

pub fn split_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    for (index, character) in buffer.char_indices() {
        if !matches!(character, '。' | '！' | '？' | '\n') {
            continue;
        }
        let end = index + character.len_utf8();
        let sentence = buffer[start..end].trim();
        if !sentence.is_empty() {
            sentences.push(sentence.to_owned());
        }
        start = end;
    }
    if start > 0 {
        buffer.drain(..start);
    }
    while buffer.chars().count() > MAX_SENTENCE_CHARS {
        let cut = buffer
            .char_indices()
            .nth(MAX_SENTENCE_CHARS)
            .map_or(buffer.len(), |(index, _)| index);
        let sentence = buffer[..cut].trim().to_owned();
        buffer.drain(..cut);
        if !sentence.is_empty() {
            sentences.push(sentence);
        }
    }
    sentences
}

pub fn flush_sentences(buffer: &mut String) -> Option<String> {
    let text = buffer.trim().to_owned();
    buffer.clear();
    (!text.is_empty()).then_some(text)
}

pub fn audio_events(
    companion: &str,
    utterance_id: &str,
    generation_id: u64,
    sequence_no: u32,
    pcm: &[u8],
) -> Vec<VoiceOutputEvent> {
    if pcm.is_empty() {
        return Vec::new();
    }
    let total = pcm.len().div_ceil(AUDIO_CHUNK_BYTES);
    pcm.chunks(AUDIO_CHUNK_BYTES)
        .enumerate()
        .map(|(index, chunk)| {
            VoiceOutputEvent::Chunk(AudioChunk {
                utterance_id: utterance_id.to_owned(),
                generation_id,
                sequence_no,
                pcm_data: chunk.to_vec(),
                sample_rate: SAMPLE_RATE,
                is_final_chunk: index + 1 == total,
                companion_id: companion.to_owned(),
            })
        })
        .collect()
}

pub fn finished_event(
    companion: &str,
    utterance_id: &str,
    generation_id: u64,
    total_sentences: u32,
) -> VoiceOutputEvent {
    VoiceOutputEvent::Finished(UtteranceFinished {
        utterance_id: utterance_id.to_owned(),
        generation_id,
        total_sentences,
        companion_id: companion.to_owned(),
    })
}

pub fn flush_event(
    companion: &str,
    utterance_id: &str,
    flush_generation_id: u64,
    reason: FlushReason,
) -> VoiceOutputEvent {
    VoiceOutputEvent::Flush(FlushControl {
        utterance_id: utterance_id.to_owned(),
        flush_generation_id,
        reason,
        companion_id: companion.to_owned(),
    })
}

pub async fn synthesize(
    engine: &dyn TtsEngine,
    text: String,
    voice: String,
) -> HttpResult<(Vec<u8>, u32)> {
    match tokio::time::timeout(SYNTHESIS_TIMEOUT, engine.synthesize(text, voice)).await {
        Err(_) => Err(VoiceHttpError::Transport(
            "the TTS connection timed out".into(),
        )),
        Ok(outcome) => outcome,
    }
}

/// Speaks `text` outside any turn. The speech floor is not taken and no conversation
/// is touched: this is the probe a settings screen uses to hear a TTS connection.
pub async fn synthesize_for_probe(
    source: Arc<dyn TtsEngine>,
    text: &str,
    voice: String,
) -> HttpResult<(Vec<u8>, u32)> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(VoiceHttpError::InvalidInput("text is required".into()));
    }
    if text.chars().count() > MAX_PROBE_CHARS {
        return Err(VoiceHttpError::LimitExceeded(format!(
            "text is limited to {MAX_PROBE_CHARS} characters"
        )));
    }
    synthesize(&*source, text, voice).await
}
