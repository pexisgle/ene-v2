//! Speech floor, sentence-level synthesis queueing, audio streams, and STT deduplication.

pub mod events;

pub use events::{
    AudioSubscription, Dispatch, EndedUtterance, InputBinding, MAX_PROBE_CHARS, MAX_SENTENCE_CHARS,
    MAX_STT_BYTES, MIN_STT_BYTES, MIN_STT_RMS, STT_SILENCE_WINDOW, STT_TIMEOUT, VOICE_UNAVAILABLE,
    audio_event, audio_events, finished_event, flush_event, flush_sentences, split_sentences,
    synthesize, synthesize_for_probe,
};

use crate::{
    engine::TtsEngine,
    error::{BoxFuture, HttpResult, VoiceSessionError},
    pcm::{ResamplerCache, wire_pcm},
};
use ene_rpc as w;
use futures_util::{StreamExt, stream::FuturesOrdered};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{Notify, mpsc};
use tokio_util::task::AbortOnDropHandle;

const MAX_SYNTHESIS_SENTENCES: usize = 3;
const TRANSCRIPTION_MEMORY: usize = 128;
const MAX_SENTENCES: u32 = 64;
const AUDIO_QUEUE: usize = 32;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone)]
struct Stream {
    subscription: String,
    audio: mpsc::Sender<w::AudioOutputEvent>,
    control: mpsc::UnboundedSender<w::AudioOutputEvent>,
}

#[derive(Clone)]
struct Utterance {
    id: String,
    generation_id: u64,
    next_sequence: u32,
    finished: bool,
    voice: String,
    source: Arc<dyn TtsEngine>,
    buffer: String,
}

#[derive(Clone, Default)]
struct Owner {
    client_id: String,
    generation_id: u64,
    utterance: Option<Utterance>,
}

struct Work {
    companion: String,
    generation_id: u64,
    pending: VecDeque<Sentence>,
    /// Sentences enqueued for this utterance that synthesis has not resolved.
    outstanding: u32,
    /// `Some(total_sentences)` once the turn ended and named the count.
    ended: Option<u32>,
    notify: Arc<Notify>,
    task: Option<AbortOnDropHandle<()>>,
}

impl Work {
    fn new(companion: &str, generation_id: u64) -> Self {
        Self {
            companion: companion.to_owned(),
            generation_id,
            pending: VecDeque::new(),
            outstanding: 0,
            ended: None,
            notify: Arc::new(Notify::new()),
            task: None,
        }
    }

    /// Queues sentences for synthesis. Each one keeps the utterance open until
    /// its synthesis resolves, so the count never drifts from the queue.
    fn enqueue(&mut self, jobs: Vec<Sentence>) {
        self.outstanding += jobs.len() as u32;
        self.pending.extend(jobs);
    }
}

#[derive(Default)]
struct State {
    next_generation: u64,
    floor: Option<String>,
    streams: HashMap<String, Stream>,
    owners: HashMap<String, Owner>,
    works: HashMap<String, Work>,
    transcriptions: VecDeque<String>,
}

impl State {
    fn cancel(&mut self, utterance_id: &str) {
        self.works.remove(utterance_id);
    }
}

struct Sentence {
    sequence_no: u32,
    text: String,
    source: Arc<dyn TtsEngine>,
    voice: String,
}

fn sentence(utterance: &Utterance, sequence_no: u32, text: String) -> Sentence {
    Sentence {
        sequence_no,
        text,
        source: Arc::clone(&utterance.source),
        voice: utterance.voice.clone(),
    }
}

/// Async speech floor, sentence synthesis scheduler, and audio output stream registry.
#[derive(Clone)]
pub struct VoicePipeline {
    inner: Arc<Mutex<State>>,
    runtime: tokio::runtime::Handle,
}

impl fmt::Debug for VoicePipeline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoicePipeline")
            .finish_non_exhaustive()
    }
}

impl VoicePipeline {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::default(),
            runtime,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.inner)
    }

    pub fn register(&self, client: &str) -> AudioSubscription {
        let (audio, receiver) = mpsc::channel(AUDIO_QUEUE);
        let (control, control_receiver) = mpsc::unbounded_channel();
        let subscription_id = uuid::Uuid::new_v4().to_string();
        let stream = Stream {
            subscription: subscription_id.clone(),
            audio,
            control,
        };
        self.lock().streams.insert(client.to_owned(), stream);
        AudioSubscription {
            subscription_id,
            receiver,
            control: control_receiver,
        }
    }

    pub fn unregister(&self, client: &str, subscription: &str) {
        let mut state = self.lock();
        if state
            .streams
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            state.streams.remove(client);
        }
    }

    /// Records the client that owns this companion's audio output.
    pub fn publish(&self, companion: &str, client: &str) {
        let mut state = self.lock();
        let owner = state.owners.entry(companion.to_owned()).or_default();
        owner.client_id = client.to_owned();
    }

    /// Binds `companion` output to `client` if that client currently holds an audio stream.
    pub fn activate_client_if_subscribed(&self, companion: &str, client: &str) {
        let mut state = self.lock();
        if state.streams.contains_key(client) {
            let owner = state.owners.entry(companion.to_owned()).or_default();
            owner.client_id = client.to_owned();
        }
    }

    pub fn owns(&self, companion: &str) -> bool {
        self.lock().owners.contains_key(companion)
    }

    pub fn streaming(&self, companion: &str) -> bool {
        self.lock().owners.get(companion).is_some_and(|owner| {
            owner
                .utterance
                .as_ref()
                .is_some_and(|utterance| !utterance.finished)
        })
    }

    pub fn floor_holder(&self) -> Option<String> {
        self.lock().floor.clone()
    }

    pub fn begin(&self, companion: &str, source: Arc<dyn TtsEngine>, voice: String) -> bool {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return false;
        }
        finish(&mut state, companion, Some(w::FlushReason::Cancelled));
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
    pub fn play(&self, companion: &str, pcm: &[u8]) -> Result<(), VoiceSessionError> {
        let mut state = self.lock();
        let Some(owner) = state.owners.get(companion) else {
            return Err(VoiceSessionError::NoSubscriber);
        };
        let client_id = owner.client_id.clone();
        state.next_generation += 1;
        let generation_id = state.next_generation;
        let utterance_id = uuid::Uuid::new_v4().to_string();
        let events = audio_events(companion, &utterance_id, generation_id, 0, pcm);
        let Some(stream) = state.streams.get(&client_id) else {
            return Err(VoiceSessionError::StreamMissing);
        };
        for event in events {
            stream.audio.try_send(event).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => VoiceSessionError::AudioQueueFull,
                mpsc::error::TrySendError::Closed(_) => VoiceSessionError::StreamClosed,
            })?;
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
            finish(&mut state, companion, Some(w::FlushReason::Cancelled));
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
            finish(&mut state, companion, Some(w::FlushReason::Cancelled));
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

    fn announce(&self, state: &mut State, utterance_id: &str) {
        let Some(work) = state.works.get_mut(utterance_id) else {
            return;
        };
        if work.outstanding > 0 {
            return;
        }
        let Some(total) = work.ended.take() else {
            return;
        };
        let companion = work.companion.clone();
        let generation_id = work.generation_id;
        self.send(
            state,
            &companion,
            generation_id,
            vec![finished_event(
                &companion,
                utterance_id,
                generation_id,
                total,
            )],
            utterance_id,
        );
    }

    pub fn flush(&self, companion: &str, reason: w::FlushReason) -> Result<(), VoiceSessionError> {
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
        finish(&mut state, companion, Some(w::FlushReason::ClientMoved));
        state.owners.remove(companion);
    }

    fn pump(&self, state: &mut State, utterance_id: &str) {
        let Some(work) = state.works.get_mut(utterance_id) else {
            return;
        };
        work.notify.notify_one();
        if work.task.is_some() {
            return;
        }
        let notify = Arc::clone(&work.notify);
        let runner = self.clone();
        let id = utterance_id.to_owned();
        work.task = Some(AbortOnDropHandle::new(self.runtime.spawn(async move {
            let mut cache: ResamplerCache = None;
            let mut pending =
                FuturesOrdered::<BoxFuture<'static, (u32, HttpResult<(Vec<u8>, u32)>)>>::new();
            loop {
                let drained = {
                    let mut state = runner.lock();
                    let Some(work) = state.works.get_mut(&id) else {
                        return;
                    };
                    while pending.len() < MAX_SYNTHESIS_SENTENCES {
                        let Some(sentence) = work.pending.pop_front() else {
                            break;
                        };
                        pending.push_back(Box::pin(async move {
                            let outcome =
                                synthesize(&*sentence.source, sentence.text, sentence.voice).await;
                            (sentence.sequence_no, outcome)
                        }));
                    }
                    let drained = pending.is_empty();
                    if drained {
                        work.task.take();
                        retire(&mut state, &id);
                    }
                    drained
                };
                if drained {
                    return;
                }
                tokio::select! {
                    _ = notify.notified() => {},
                    Some((sequence, outcome)) = pending.next(), if !pending.is_empty() => {
                        let outcome = outcome.and_then(|(pcm, rate)| wire_pcm(pcm, rate, &mut cache));
                        runner.complete(&id, sequence, outcome);
                    }
                }
            }
        })));
    }

    fn complete(&self, utterance_id: &str, sequence_no: u32, outcome: HttpResult<Vec<u8>>) {
        let mut state = self.lock();
        let Some(work) = state.works.get(utterance_id) else {
            return;
        };
        let companion = work.companion.clone();
        let generation_id = work.generation_id;
        let events = match outcome {
            Ok(pcm) => audio_events(&companion, utterance_id, generation_id, sequence_no, &pcm),
            Err(reason) => {
                tracing::debug!(%sequence_no, %reason, "sentence synthesis failed; the utterance continues without it");
                Vec::new()
            }
        };
        self.send(&mut state, &companion, generation_id, events, utterance_id);
        let Some(work) = state.works.get_mut(utterance_id) else {
            return;
        };
        work.outstanding -= 1;
        self.announce(&mut state, utterance_id);
    }

    fn send(
        &self,
        state: &mut State,
        companion: &str,
        generation_id: u64,
        events: Vec<w::AudioOutputEvent>,
        utterance_id: &str,
    ) {
        for event in events {
            match deliver_state(state, companion, generation_id, event) {
                Dispatch::Sent => {}
                Dispatch::Stale => {
                    state.cancel(utterance_id);
                    return;
                }
                Dispatch::Full | Dispatch::Closed => {
                    fail_owner(state, companion);
                    tracing::debug!(%companion, "the audio client stalled; its utterance was cancelled");
                    return;
                }
            }
        }
    }

    pub fn accept_transcription(&self, transcription_id: &str) -> bool {
        if transcription_id.is_empty() {
            return true;
        }
        let mut state = self.lock();
        if state
            .transcriptions
            .iter()
            .any(|known| known == transcription_id)
        {
            return false;
        }
        state.transcriptions.push_back(transcription_id.to_owned());
        if state.transcriptions.len() > TRANSCRIPTION_MEMORY {
            state.transcriptions.pop_front();
        }
        true
    }

    /// Returns the active utterance generation id for `companion`, if subscribed.
    pub fn generation_of(&self, companion: &str) -> Option<u64> {
        self.lock()
            .owners
            .get(companion)
            .map(|owner| owner.generation_id)
    }

    /// Attempts direct event delivery against `generation_id` without modifying pending work.
    pub fn deliver_raw(
        &self,
        companion: &str,
        generation_id: u64,
        event: w::AudioOutputEvent,
    ) -> Dispatch {
        let mut state = self.lock();
        deliver_state(&mut state, companion, generation_id, event)
    }

    /// Returns whether any synthesis work queue is currently active.
    pub fn has_active_work(&self) -> bool {
        !self.lock().works.is_empty()
    }

    /// Returns whether `companion` currently owns an active audio route.
    pub fn has_owner(&self, companion: &str) -> bool {
        self.lock().owners.contains_key(companion)
    }
}

fn finish(state: &mut State, companion: &str, reason: Option<w::FlushReason>) {
    let Some(owner) = state.owners.get_mut(companion) else {
        return;
    };
    let Some(utterance) = owner.utterance.take() else {
        return;
    };
    if let Some(reason) = reason
        && let Some(stream) = state.streams.get(&owner.client_id)
    {
        let _ = stream.control.send(flush_event(
            companion,
            &utterance.id,
            owner.generation_id,
            reason,
        ));
    }
    owner.generation_id = 0;
    state.cancel(&utterance.id);
    if state.floor.as_deref() == Some(companion) {
        state.floor = None;
    }
}

fn fail_owner(state: &mut State, companion: &str) {
    finish(state, companion, Some(w::FlushReason::Cancelled));
    state.owners.remove(companion);
    if state.floor.as_deref() == Some(companion) {
        state.floor = None;
    }
}

fn retire(state: &mut State, utterance_id: &str) {
    let Some(companion) = state
        .works
        .get(utterance_id)
        .map(|work| work.companion.clone())
    else {
        return;
    };
    let Some(owner) = state.owners.get_mut(&companion) else {
        return;
    };
    if owner
        .utterance
        .as_ref()
        .is_none_or(|utterance| utterance.id != utterance_id || !utterance.finished)
    {
        return;
    }
    owner.utterance = None;
    state.works.remove(utterance_id);
}

fn deliver_state(
    state: &mut State,
    companion: &str,
    generation_id: u64,
    event: w::AudioOutputEvent,
) -> Dispatch {
    let Some(owner) = state.owners.get(companion) else {
        return Dispatch::Stale;
    };
    if owner
        .utterance
        .as_ref()
        .is_none_or(|utterance| utterance.generation_id != generation_id)
    {
        return Dispatch::Stale;
    }
    let Some(stream) = state.streams.get(&owner.client_id) else {
        return Dispatch::Stale;
    };
    match stream.audio.try_send(event) {
        Ok(()) => Dispatch::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => Dispatch::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => Dispatch::Closed,
    }
}

#[cfg(test)]
mod tests;
