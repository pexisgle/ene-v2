//! Voice domain events -> Connect RPC wire messages.
//!
//! `ene-voice` produces transport-agnostic events; this module is the only
//! place they become the contract's `AudioOutputEvent`.

use ene_rpc as w;
use ene_voice::{FlushReason, VoiceOutputEvent};

/// Converts one voice output event into its wire message.
pub(super) fn to_wire_event(event: VoiceOutputEvent) -> w::AudioOutputEvent {
    let inner = match event {
        VoiceOutputEvent::Chunk(chunk) => {
            w::audio_output_event::Event::from(w::AudioChunkEnvelope {
                utterance_id: chunk.utterance_id,
                generation_id: chunk.generation_id,
                sequence_no: chunk.sequence_no,
                pcm_data: chunk.pcm_data,
                sample_rate: chunk.sample_rate,
                is_final_chunk: chunk.is_final_chunk,
                companion_id: chunk.companion_id,
                ..Default::default()
            })
        }
        VoiceOutputEvent::Flush(flush) => w::audio_output_event::Event::from(w::FlushControl {
            utterance_id: flush.utterance_id,
            flush_generation_id: flush.flush_generation_id,
            reason: flush_reason(flush.reason).into(),
            companion_id: flush.companion_id,
            ..Default::default()
        }),
        VoiceOutputEvent::Finished(finished) => {
            w::audio_output_event::Event::from(w::UtteranceFinished {
                utterance_id: finished.utterance_id,
                generation_id: finished.generation_id,
                total_sentences: finished.total_sentences,
                companion_id: finished.companion_id,
                ..Default::default()
            })
        }
    };
    w::AudioOutputEvent {
        event: Some(inner),
        ..Default::default()
    }
}

/// Converts a voice flush reason into its wire value.
pub(crate) fn flush_reason(reason: FlushReason) -> w::FlushReason {
    match reason {
        FlushReason::Unspecified => w::FlushReason::Unspecified,
        FlushReason::BargeIn => w::FlushReason::BargeIn,
        FlushReason::Cancelled => w::FlushReason::Cancelled,
        FlushReason::ClientMoved => w::FlushReason::ClientMoved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_voice::{AudioChunk, FlushControl, UtteranceFinished};

    #[test]
    fn a_chunk_keeps_its_identity_and_payload() {
        let event = to_wire_event(VoiceOutputEvent::Chunk(AudioChunk {
            utterance_id: "utt".into(),
            generation_id: 7,
            sequence_no: 3,
            pcm_data: vec![1, 2, 3],
            sample_rate: 24_000,
            is_final_chunk: true,
            companion_id: "ene".into(),
        }));
        let Some(w::audio_output_event::Event::Chunk(chunk)) = event.event else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.utterance_id, "utt");
        assert_eq!(chunk.generation_id, 7);
        assert_eq!(chunk.sequence_no, 3);
        assert_eq!(chunk.pcm_data, vec![1, 2, 3]);
        assert_eq!(chunk.sample_rate, 24_000);
        assert!(chunk.is_final_chunk);
        assert_eq!(chunk.companion_id, "ene");
    }

    #[test]
    fn a_flush_carries_its_reason() {
        let event = to_wire_event(VoiceOutputEvent::Flush(FlushControl {
            utterance_id: "utt".into(),
            flush_generation_id: 9,
            reason: FlushReason::BargeIn,
            companion_id: "ene".into(),
        }));
        let Some(w::audio_output_event::Event::Flush(flush)) = event.event else {
            panic!("expected a flush");
        };
        assert_eq!(flush.flush_generation_id, 9);
        assert_eq!(flush.reason, w::FlushReason::BargeIn);
    }

    #[test]
    fn a_finished_event_carries_the_sentence_count() {
        let event = to_wire_event(VoiceOutputEvent::Finished(UtteranceFinished {
            utterance_id: "utt".into(),
            generation_id: 2,
            total_sentences: 4,
            companion_id: "ene".into(),
        }));
        let Some(w::audio_output_event::Event::Finished(finished)) = event.event else {
            panic!("expected finished");
        };
        assert_eq!(finished.total_sentences, 4);
        assert_eq!(finished.generation_id, 2);
    }

    #[test]
    fn every_flush_reason_maps_to_its_wire_value() {
        for (domain, wire) in [
            (FlushReason::Unspecified, w::FlushReason::Unspecified),
            (FlushReason::BargeIn, w::FlushReason::BargeIn),
            (FlushReason::Cancelled, w::FlushReason::Cancelled),
            (FlushReason::ClientMoved, w::FlushReason::ClientMoved),
        ] {
            assert_eq!(flush_reason(domain), wire);
        }
    }
}
