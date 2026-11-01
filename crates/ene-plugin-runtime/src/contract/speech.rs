//! Speech chunk validation.
use super::Events;
use crate::{Error, Result, limits::MAX_CHUNK};
use ene_plugin::AudioChunk;

impl Events {
    pub(super) fn accept_audio(&mut self, chunk: &AudioChunk) -> Result<()> {
        if self.speech.as_ref() != Some(&(chunk.utterance_id.clone(), chunk.generation))
            || chunk.sequence != self.audio_sequence
            || chunk.format != "pcm_s16le"
            || !(8000..=192000).contains(&chunk.sample_rate)
            || !(1..=8).contains(&chunk.channels)
        {
            return Err(Error::Protocol);
        }
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data_base64)
            .map_err(|_| Error::Protocol)?;
        if bytes.len() > MAX_CHUNK || bytes.len() % (2 * chunk.channels as usize) != 0 {
            return Err(Error::Protocol);
        }
        self.audio_sequence += 1;
        self.terminal = chunk.done;
        Ok(())
    }
}
