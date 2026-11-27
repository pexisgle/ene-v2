//! Speech-floor turn lifecycle: beginning, speaking into, and ending an utterance.

use super::*;

const MAX_SENTENCES: u32 = 64;

fn sentence(utterance: &Utterance, sequence_no: u32, text: String) -> Sentence {
    Sentence {
        sequence_no,
        text,
        source: Arc::clone(&utterance.source),
        voice: utterance.voice.clone(),
    }
}

impl VoicePipeline {
    pub fn begin(&self, companion: &str, source: Arc<dyn TtsEngine>, voice: String) -> bool {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return false;
        }
        finish(&mut state, companion, Some(FlushReason::Cancelled));
        state.next_generation += 1;
        let generation_id = state.next_generation;
        let id = uuid::Uuid::new_v4().to_string();
        let Some(owner) = state.owners.get_mut(companion) else {
            return false;
        };
        owner.generation_id = generation_id;
        owner.utterance = Some(Utterance {
            id: id.clone(),
            generation_id,
            next_sequence: 0,
            finished: false,
            voice,
            source,
            buffer: String::new(),
        });
        state
            .works
            .insert(id.clone(), Work::new(companion, generation_id));
        if state
            .floor
            .as_deref()
            .is_none_or(|holder| holder == companion)
        {
            state.floor = Some(companion.to_owned());
        }
        true
    }

    /// Plays one already-synthesized utterance without taking the speech floor.
    pub fn play(
        &self,
        companion: &str,
        client: &str,
        subscription: &str,
        pcm: &[u8],
    ) -> Result<(), VoiceSessionError> {
        let State {
            next_generation,
            owners,
            streams,
            ..
        } = &mut *self.lock();
        if owners
            .get(companion)
            .is_none_or(|owner| owner.client_id != client)
        {
            return Err(VoiceSessionError::NoSubscriber);
        }
        let Some(stream) = streams
            .get(client)
            .filter(|stream| stream.subscription == subscription)
        else {
            return Err(VoiceSessionError::StreamMissing);
        };
        let count = pcm.len().div_ceil(events::AUDIO_CHUNK_BYTES) + 1;
        let permits = stream
            .audio
            .try_reserve_many(count)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => VoiceSessionError::AudioQueueFull,
                mpsc::error::TrySendError::Closed(_) => VoiceSessionError::StreamClosed,
            })?;
        *next_generation += 1;
        let generation_id = *next_generation;
        let utterance_id = uuid::Uuid::new_v4().to_string();
        let events = audio_events(companion, &utterance_id, generation_id, 0, pcm)
            .into_iter()
            .chain(std::iter::once(finished_event(
                companion,
                &utterance_id,
                generation_id,
                1,
            )));
        for (permit, event) in permits.zip(events) {
            permit.send(event);
        }
        Ok(())
    }

    pub fn speak(&self, companion: &str, text: &str) {
        let mut state = self.lock();
        let mut jobs = Vec::new();
        let mut overflow = false;
        let utterance_id;
        {
            let Some(utterance) = state
                .owners
                .get_mut(companion)
                .and_then(|owner| owner.utterance.as_mut())
            else {
                return;
            };
            utterance_id = utterance.id.clone();
            utterance.buffer.push_str(text);
            for text in split_sentences(&mut utterance.buffer) {
                if utterance.next_sequence >= MAX_SENTENCES {
                    overflow = true;
                    break;
                }
                let sequence_no = utterance.next_sequence;
                utterance.next_sequence += 1;
                jobs.push(sentence(utterance, sequence_no, text));
            }
        }
        if overflow {
            finish(&mut state, companion, Some(FlushReason::Cancelled));
            return;
        }
        if let Some(work) = state.works.get_mut(&utterance_id) {
            work.enqueue(jobs);
        }
        self.pump(&mut state, &utterance_id);
    }

    /// Ends the turn: the buffered tail becomes the last sentence and the floor
    /// is released.
    pub fn end_turn(&self, companion: &str) -> Option<EndedUtterance> {
        let mut state = self.lock();
        if state.floor.as_deref() != Some(companion) {
            return None;
        }
        let Some(utterance_id) = state
            .owners
            .get(companion)
            .and_then(|owner| owner.utterance.as_ref())
            .map(|utterance| utterance.id.clone())
        else {
            finish(&mut state, companion, None);
            return None;
        };
        let mut jobs = Vec::new();
        let mut overflow = false;
        let total;
        {
            let utterance = state
                .owners
                .get_mut(companion)
                .and_then(|owner| owner.utterance.as_mut())?;
            if utterance.finished {
                return None;
            }
            if let Some(text) = flush_sentences(&mut utterance.buffer) {
                if utterance.next_sequence >= MAX_SENTENCES {
                    overflow = true;
                } else {
                    let sequence_no = utterance.next_sequence;
                    utterance.next_sequence += 1;
                    jobs.push(sentence(utterance, sequence_no, text));
                }
            }
            total = utterance.next_sequence;
            if !overflow {
                utterance.finished = true;
            }
        }
        if overflow {
            finish(&mut state, companion, Some(FlushReason::Cancelled));
            return None;
        }
        let work = state.works.get_mut(&utterance_id)?;
        let generation_id = work.generation_id;
        work.enqueue(jobs);
        work.ended = Some(total);
        if state.floor.as_deref() == Some(companion) {
            state.floor = None;
        }
        let client_id = state
            .owners
            .get(companion)
            .map(|owner| owner.client_id.clone())?;
        self.announce(&mut state, &utterance_id);
        if total == 0 {
            finish(&mut state, companion, None);
            return None;
        }
        self.pump(&mut state, &utterance_id);
        Some(EndedUtterance {
            client_id,
            utterance_id,
            generation_id,
            total_sentences: total,
        })
    }

    pub fn flush(&self, companion: &str, reason: FlushReason) -> Result<(), VoiceSessionError> {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return Err(VoiceSessionError::NoOwner);
        }
        finish(&mut state, companion, Some(reason));
        Ok(())
    }

    pub fn client_moved(&self, companion: &str, client: &str) {
        let mut state = self.lock();
        let owns = state
            .owners
            .get(companion)
            .is_some_and(|owner| owner.client_id == client);
        if !owns {
            return;
        }
        finish(&mut state, companion, Some(FlushReason::ClientMoved));
        state.owners.remove(companion);
    }
}
