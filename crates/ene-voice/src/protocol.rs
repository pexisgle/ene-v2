//! Pure voice output events.
//!
//! These are the values a voice session produces. The RPC layer converts them
//! into its own wire messages at the stream boundary, so this crate never
//! depends on the protocol contract.

/// Why a voice output stream was flushed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlushReason {
    /// No reason was given.
    #[default]
    Unspecified,
    /// The user started speaking over the utterance.
    BargeIn,
    /// The utterance was cancelled.
    Cancelled,
    /// The client moved to another companion.
    ClientMoved,
}

/// One slice of synthesized PCM for an utterance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioChunk {
    /// The utterance this chunk belongs to.
    pub utterance_id: String,
    /// Monotonic generation counter; older generations are dropped.
    pub generation_id: u64,
    /// The chunk's index within the utterance.
    pub sequence_no: u32,
    /// Raw PCM bytes.
    pub pcm_data: Vec<u8>,
    /// Sample rate of `pcm_data`.
    pub sample_rate: u32,
    /// Whether this is the utterance's last chunk.
    pub is_final_chunk: bool,
    /// The companion that spoke.
    pub companion_id: String,
}

/// The end of an utterance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UtteranceFinished {
    /// The utterance that finished.
    pub utterance_id: String,
    /// The generation that finished.
    pub generation_id: u64,
    /// How many sentences were spoken.
    pub total_sentences: u32,
    /// The companion that spoke.
    pub companion_id: String,
}

/// An instruction to discard buffered audio.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushControl {
    /// The utterance being flushed.
    pub utterance_id: String,
    /// The generation to flush up to.
    pub flush_generation_id: u64,
    /// Why the flush happened.
    pub reason: FlushReason,
    /// The companion whose audio is flushed.
    pub companion_id: String,
}

/// Anything a voice session emits on its output stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceOutputEvent {
    /// A slice of PCM audio.
    Chunk(AudioChunk),
    /// An instruction to drop buffered audio.
    Flush(FlushControl),
    /// The end of an utterance.
    Finished(UtteranceFinished),
}

impl Default for VoiceOutputEvent {
    fn default() -> Self {
        Self::Chunk(AudioChunk::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_domain::VoiceBinding;

    #[test]
    fn flush_reasons_are_distinct_and_default_to_unspecified() {
        assert_eq!(FlushReason::default(), FlushReason::Unspecified);
        assert_ne!(FlushReason::BargeIn, FlushReason::Cancelled);
        assert_ne!(FlushReason::ClientMoved, FlushReason::BargeIn);
    }

    #[test]
    fn a_voice_binding_round_trips_through_json() {
        let binding = VoiceBinding {
            connection_id: Some("tts".into()),
            voice_id: Some("zundamon/amaama".into()),
            speed: Some(1.25),
            pitch: Some(-0.5),
            style: Some("normal".into()),
        };
        let json = serde_json::to_string(&binding).expect("serialize");
        let back: VoiceBinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, binding);
    }

    #[test]
    fn a_voice_binding_tolerates_absent_fields() {
        let back: VoiceBinding = serde_json::from_str("{}").expect("deserialize empty");
        assert_eq!(back, VoiceBinding::default());
    }
}
