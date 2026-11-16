//! Host-side voice pipeline: recognition input, sentence-level synthesis
//! output and the speech floor (docs/design/voice-avatar.md §4–§5,
//! docs/api.md §6–§7). Recognition runs in the Connect layer; synthesis tasks
//! run on the host runtime and release audio in `sequence_no` order.
//!
//! A turn streams text sentence by sentence while the host synthesizes up to
//! [`MAX_SYNTHESIS_SENTENCES`] ahead. The host releases the speech floor when
//! the turn ends and emits `UtteranceFinished` only once every sentence queued
//! for the utterance has resolved, so the event stays the last audio event of
//! the utterance. A sentence whose synthesis fails adds no audio and nothing
//! waits for it. `generation_id` and `FlushControl` drop the chunks of an older
//! generation after a barge-in; `sequence_no` only carries the enqueue order.
use super::{
    agent,
    db::{self, Result, Store},
    delegation,
    error::HostError,
    model::Credentials,
    notify::Notifier,
    resources::HostRuntime,
    settings::{Connection, ConnectionKind},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use futures_util::{StreamExt, stream::FuturesOrdered};
use rig_core::conversation::{Aside, Author, RequestId};
use rig_ecs::prelude::*;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::{Notify, mpsc};
use tokio_util::task::AbortOnDropHandle;

pub(crate) const SAMPLE_RATE: u32 = 24_000;
pub(crate) const STT_SAMPLE_RATES: [u32; 2] = [24_000, 16_000];
const MAX_SYNTHESIS_SENTENCES: usize = 3;
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const STT_TIMEOUT: Duration = Duration::from_secs(30);
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_SENTENCE_CHARS: usize = 240;
pub(crate) const STT_SILENCE_WINDOW: Duration = Duration::from_millis(700);
pub(crate) const MIN_STT_BYTES: usize = 3_200;
pub(crate) const MAX_STT_BYTES: usize = 48_000 * 20;
const TRANSCRIPTION_MEMORY: usize = 128;
const MAX_SENTENCES: u32 = 64;
const AUDIO_QUEUE: usize = 32;

pub(crate) const VOICE_UNAVAILABLE: &str = "voice requires a configured TTS/STT connection";

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type HttpResult<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SetupError {
    Missing,
    Disabled(String),
    Broken(String),
    Unsupported(String),
}

pub(crate) trait TtsEngine: Send + Sync + 'static {
    fn synthesize<'a>(
        &'a self,
        text: String,
        voice: String,
    ) -> BoxFuture<'a, HttpResult<(Vec<u8>, u32)>>;
}

pub(crate) trait SttEngine: Send + Sync + 'static {
    fn transcribe<'a>(
        &'a self,
        pcm: Vec<u8>,
        sample_rate: u32,
    ) -> BoxFuture<'a, HttpResult<String>>;
}

/// Engines a test injects to stand in for the plugin path.
#[derive(Resource, Default)]
pub(crate) struct Engines {
    pub(crate) tts: Option<Arc<dyn TtsEngine>>,
    pub(crate) stt: Option<Arc<dyn SttEngine>>,
}

pub(crate) fn tts_source(world: &World) -> std::result::Result<Arc<dyn TtsEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.tts.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let connection = voice_connection(world, ConnectionKind::Tts)?;
    let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Tts)? else {
        return Err(SetupError::Unsupported(format!(
            "the TTS adapter `{}` is not served by an installed plugin",
            connection.endpoint().adapter
        )));
    };
    Ok(Arc::new(engine))
}

pub(crate) fn stt_source(world: &World) -> std::result::Result<Arc<dyn SttEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.stt.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let connection = voice_connection(world, ConnectionKind::Stt)?;
    let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Stt)? else {
        return Err(SetupError::Unsupported(format!(
            "the STT adapter `{}` is not served by an installed plugin",
            connection.endpoint().adapter
        )));
    };
    Ok(Arc::new(engine))
}

/// The plugin engine for `connection`, when a plugin serves its adapter.
fn plugin_engine(
    world: &World,
    connection: &Connection,
    kind: ConnectionKind,
) -> std::result::Result<Option<plugin::PluginSpeech>, SetupError> {
    let Some(providers) = world.get_resource::<crate::host::providers::Providers>() else {
        return Ok(None);
    };
    plugin::PluginSpeech::new(
        providers,
        connection,
        world.get_resource::<Credentials>(),
        kind,
    )
}

fn voice_connection(
    world: &World,
    kind: ConnectionKind,
) -> std::result::Result<Connection, SetupError> {
    let Some(store) = world.get_resource::<Store>() else {
        return Err(SetupError::Broken("the host state is not available".into()));
    };
    let settings = store
        .settings()
        .map_err(|error| SetupError::Broken(error.to_string()))?;
    let Some(connection) = settings
        .connections
        .into_iter()
        .find(|connection| connection.kind == kind)
    else {
        return Err(SetupError::Missing);
    };
    if !connection.enabled {
        return Err(SetupError::Disabled(format!(
            "the {} connection `{}` is disabled",
            kind.as_str(),
            connection.id
        )));
    }
    Ok(connection)
}

type ResamplerCache = Option<(u32, u32, rubato::Fft<f32>)>;

fn wire_pcm(pcm: Vec<u8>, sample_rate: u32, cache: &mut ResamplerCache) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err("the TTS engine returned an odd number of PCM bytes".into());
    }
    if sample_rate == 0 {
        return Err("the TTS engine returned audio without a sample rate".into());
    }
    if sample_rate == SAMPLE_RATE {
        return Ok(pcm);
    }
    resample_mono(&pcm, sample_rate, SAMPLE_RATE, cache)
}

fn resample_mono(
    pcm: &[u8],
    from: u32,
    to: u32,
    cache: &mut ResamplerCache,
) -> HttpResult<Vec<u8>> {
    use rubato::{Fft, FixedSync, Resampler};
    if from == 0 {
        return Err("the TTS engine returned audio without a sample rate".into());
    }
    if from == to {
        return Ok(pcm.to_vec());
    }
    let samples: Vec<f32> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect();
    if samples.is_empty() {
        return Ok(Vec::new());
    }
    let expected = ((samples.len() as f64) * f64::from(to) / f64::from(from)).round() as usize;
    let resampler = match cache {
        Some((cached_from, cached_to, resampler)) if *cached_from == from && *cached_to == to => {
            resampler
        }
        slot => {
            let resampler = Fft::<f32>::new(from as usize, to as usize, 1_024, 1, FixedSync::Input)
                .map_err(|error| format!("the TTS sample rate cannot be converted: {error}"))?;
            &mut slot.insert((from, to, resampler)).2
        }
    };
    let input =
        rubato::audioadapter_buffers::direct::InterleavedSlice::new(&samples, 1, samples.len())
            .map_err(|error| format!("the TTS sample rate cannot be converted: {error}"))?;
    // `process_all` resets the cached filter between independent sentences.
    let mut resampled = resampler
        .process_all(&input, samples.len(), None)
        .map_err(|error| format!("the TTS sample rate cannot be converted: {error}"))?
        .take_data();
    resampled.truncate(expected);
    let mut pcm = Vec::with_capacity(resampled.len() * 2);
    for sample in resampled {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    Ok(pcm)
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

/// A finished utterance the host relays to the peers (voice-avatar.md §5.3).
struct EndedUtterance {
    client_id: String,
    utterance_id: String,
    generation_id: u64,
    total_sentences: u32,
}

#[derive(Clone, Resource)]
pub(crate) struct Voice {
    inner: Arc<Mutex<State>>,
    runtime: tokio::runtime::Handle,
}

impl Voice {
    fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::default(),
            runtime,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        super::lock(&self.inner)
    }

    fn register(&self, client: &str) -> AudioSubscription {
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

    fn unregister(&self, client: &str, subscription: &str) {
        let mut state = self.lock();
        if state
            .streams
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            state.streams.remove(client);
        }
    }

    /// The client that owns this companion's audio output.
    fn publish(&self, companion: &str, client: &str) {
        let mut state = self.lock();
        let owner = state.owners.entry(companion.to_owned()).or_default();
        owner.client_id = client.to_owned();
    }

    fn owns(&self, companion: &str) -> bool {
        self.lock().owners.contains_key(companion)
    }

    fn streaming(&self, companion: &str) -> bool {
        self.lock().owners.get(companion).is_some_and(|owner| {
            owner
                .utterance
                .as_ref()
                .is_some_and(|utterance| !utterance.finished)
        })
    }

    fn floor_holder(&self) -> Option<String> {
        self.lock().floor.clone()
    }

    fn begin(&self, companion: &str, source: Arc<dyn TtsEngine>, voice: String) -> bool {
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

    fn speak(&self, companion: &str, text: &str) {
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
    /// is released. `UtteranceFinished` waits for the audio still in flight, so
    /// it stays the last audio event of the utterance. Returns the finished
    /// utterance when the host may relay it to the peers.
    fn end_turn(&self, companion: &str) -> Option<EndedUtterance> {
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
        // A turn whose sentences are all resolved already finishes here; the
        // rest are announced by the completion that resolves the last one.
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

    /// Emits `UtteranceFinished` when the turn has ended and every sentence
    /// queued for the utterance has resolved. Nothing may follow it, so a
    /// client reads it as "the host is done with this utterance".
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
            vec![finished_event(utterance_id, generation_id, total)],
            utterance_id,
        );
    }

    fn flush(&self, companion: &str, reason: w::FlushReason) -> Result<()> {
        let mut state = self.lock();
        if !state.owners.contains_key(companion) {
            return Err(HostError::NotFound(
                ("no client owns this companion's audio output").into(),
            ));
        }
        finish(&mut state, companion, Some(reason));
        Ok(())
    }

    fn client_moved(&self, companion: &str, client: &str) {
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
        // A sentence that fails to synthesize contributes no audio and releases
        // its hold on the utterance like any other resolution.
        let events = match outcome {
            Ok(pcm) => audio_events(utterance_id, generation_id, sequence_no, &pcm),
            Err(reason) => {
                tracing::debug!(%sequence_no, %reason, "sentence synthesis failed; the utterance continues without it");
                Vec::new()
            }
        };
        self.send(&mut state, &companion, generation_id, events, utterance_id);
        // A stalled or stale delivery cancels the utterance; nothing is left to
        // announce then.
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
            match deliver(state, companion, generation_id, event) {
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

    fn accept_transcription(&self, transcription_id: &str) -> bool {
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
        let _ = stream
            .control
            .send(flush_event(&utterance.id, owner.generation_id, reason));
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

/// Forgets a finished utterance once its audio has drained. The utterance stays
/// on the owner until then, so a straggling chunk still passes the generation
/// gate; an utterance that drains mid-turn keeps its work for the next sentence.
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

fn split_sentences(buffer: &mut String) -> Vec<String> {
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
            .map(|(index, _)| index)
            .unwrap_or(buffer.len());
        let sentence = buffer[..cut].trim().to_owned();
        buffer.drain(..cut);
        if !sentence.is_empty() {
            sentences.push(sentence);
        }
    }
    sentences
}

fn flush_sentences(buffer: &mut String) -> Option<String> {
    let text = buffer.trim().to_owned();
    buffer.clear();
    (!text.is_empty()).then_some(text)
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

fn audio_events(
    utterance_id: &str,
    generation_id: u64,
    sequence_no: u32,
    pcm: &[u8],
) -> Vec<w::AudioOutputEvent> {
    if pcm.is_empty() {
        return Vec::new();
    }
    let total = pcm.len().div_ceil(AUDIO_CHUNK_BYTES);
    pcm.chunks(AUDIO_CHUNK_BYTES)
        .enumerate()
        .map(|(index, chunk)| {
            audio_event(w::AudioChunkEnvelope {
                utterance_id: utterance_id.to_owned(),
                generation_id,
                sequence_no,
                pcm_data: chunk.to_vec(),
                sample_rate: SAMPLE_RATE,
                is_final_chunk: index + 1 == total,
                ..Default::default()
            })
        })
        .collect()
}

fn finished_event(
    utterance_id: &str,
    generation_id: u64,
    total_sentences: u32,
) -> w::AudioOutputEvent {
    audio_event(w::UtteranceFinished {
        utterance_id: utterance_id.to_owned(),
        generation_id,
        total_sentences,
        ..Default::default()
    })
}

enum Dispatch {
    Sent,
    Stale,
    Full,
    Closed,
}

fn deliver(
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

async fn synthesize(
    engine: &dyn TtsEngine,
    text: String,
    voice: String,
) -> HttpResult<(Vec<u8>, u32)> {
    match tokio::time::timeout(SYNTHESIS_TIMEOUT, engine.synthesize(text, voice)).await {
        Err(_) => Err("the TTS connection timed out".into()),
        Ok(outcome) => outcome,
    }
}

fn audio_event(message: impl Into<w::audio_output_event::Event>) -> w::AudioOutputEvent {
    w::AudioOutputEvent {
        event: Some(message.into()),
        ..Default::default()
    }
}

fn flush_event(
    utterance_id: &str,
    flush_generation_id: u64,
    reason: w::FlushReason,
) -> w::AudioOutputEvent {
    audio_event(w::FlushControl {
        utterance_id: utterance_id.to_owned(),
        flush_generation_id,
        reason: reason.into(),
        ..Default::default()
    })
}

#[derive(Clone, Debug)]
pub(crate) struct InputBinding {
    pub(crate) client_id: String,
}

impl InputBinding {
    pub(crate) fn accepts(&self, client_id: &str) -> bool {
        client_id.is_empty() || client_id == self.client_id
    }
}

/// Binds an audio input stream to the client that opened it. Only the active
/// client of a companion may capture: a client that is not active has nothing
/// the head may answer.
pub(crate) fn bind_input(world: &World, client_id: &str) -> Result<InputBinding> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(
            ("client_id is required for audio input").into(),
        ));
    }
    db::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>() else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    if store.activation_for_client(client_id)?.is_none() {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    Ok(InputBinding {
        client_id: client_id.to_owned(),
    })
}

pub(crate) fn is_still_active(world: &World, binding: &InputBinding) -> bool {
    let Some(store) = world.get_resource::<Store>() else {
        return false;
    };
    store
        .activation_for_client(&binding.client_id)
        .is_ok_and(|activation| activation.is_some())
}

pub(crate) struct AudioSubscription {
    pub(crate) subscription_id: String,
    pub(crate) receiver: mpsc::Receiver<w::AudioOutputEvent>,
    pub(crate) control: mpsc::UnboundedReceiver<w::AudioOutputEvent>,
}

/// Binds an audio output stream to the client that opened it and makes it the
/// owner of every companion it is the active client of.
pub(crate) fn subscribe_audio(world: &mut World, client_id: &str) -> Result<AudioSubscription> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(("client_id is required").into()));
    }
    db::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let owned: Vec<String> = store
        .activations()?
        .into_iter()
        .filter(|activation| activation.client_id.as_deref() == Some(client_id))
        .map(|activation| activation.companion_id)
        .collect();
    if owned.is_empty() {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::Internal(
            ("the voice plugin is not installed").into(),
        ));
    };
    let subscription = voice.register(client_id);
    for companion_id in owned {
        voice.publish(&companion_id, client_id);
    }
    Ok(subscription)
}

pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.unregister(client, subscription);
    }
}

pub(crate) fn accept_transcription(
    world: &mut World,
    transcription_id: &str,
    text: &str,
    binding: &InputBinding,
) -> Result<bool> {
    let text = text.trim();
    if text.is_empty() || !is_still_active(world, binding) {
        return Ok(false);
    }
    if let Some(voice) = world.get_resource::<Voice>()
        && !voice.accept_transcription(transcription_id)
    {
        return Ok(false);
    }
    // The delegation claim of `start_work` is keyed by this transcription id.
    delegation::record(world, transcription_id);
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(false);
    };
    let companions: Vec<String> = store
        .activations()?
        .into_iter()
        .filter(|activation| activation.client_id.is_some())
        .map(|activation| activation.companion_id)
        .collect();
    let author = Author {
        request: Some(RequestId(transcription_id.to_owned())),
        ..Author::user()
    };
    for companion in companions {
        let Some(agent) = agent::agent_of(world, &companion) else {
            continue;
        };
        if let Some(mut conversation) = world.get_mut::<Conversation>(agent) {
            conversation.add_aside(Aside::new(
                "ene.transcription",
                Author::plugin("ene-host"),
                serde_json::json!({
                    "transcription_id": transcription_id,
                    "client_id": binding.client_id,
                }),
            ));
        }
        agent::deliver(
            world,
            agent,
            &companion,
            text.to_owned(),
            DeliveryMode::Steer,
            author.clone(),
        );
    }
    Ok(true)
}

pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    if !voice.owns(companion) {
        tracing::debug!(
            companion,
            "speech skipped: no client has subscribed to this companion's audio"
        );
        return;
    }
    if !voice.streaming(companion) {
        let Ok(source) = tts_source(world) else {
            tracing::debug!(companion, "speech skipped: no TTS engine is configured");
            return;
        };
        if let Some(holder) = voice.floor_holder()
            && holder != companion
        {
            if let Some(agent) = agent::agent_of(world, companion) {
                world.trigger(StopTurn {
                    entity: agent,
                    reason: Some("speech floor was taken by a peer".to_owned()),
                });
            }
            return;
        }
        let voice_name = world
            .get_resource::<Store>()
            .and_then(|store| store.companion(companion).ok().flatten())
            .and_then(|companion| companion.voice_identifier)
            .unwrap_or_default();
        if !voice.begin(companion, source, voice_name) {
            tracing::debug!(companion, "speech skipped: the utterance was not opened");
            return;
        }
        tracing::debug!(companion, "speaking this turn's reply");
    }
    voice.speak(companion, text);
}

/// Ends the speaking turn and announces it, so a peer can pick up the
/// conversation without a new user input (voice-avatar.md §5.3).
pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    let Some(ended) = voice.end_turn(companion) else {
        return;
    };
    let document = w::JsonDocument {
        schema: "ene.speech_completed.v1".into(),
        json: serde_json::json!({
            "companion_id": companion,
            "client_id": ended.client_id,
            "utterance_id": ended.utterance_id,
            "generation_id": ended.generation_id,
            "total_sentences": ended.total_sentences,
        })
        .to_string(),
        ..Default::default()
    };
    let Some(mut notifier) = world.get_resource_mut::<Notifier>() else {
        return;
    };
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "speech_completed".into(),
            payload: Some(document).into(),
            ..Default::default()
        })),
    );
}

pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::NotFound(
            ("no client owns this companion's audio output").into(),
        ));
    };
    voice.flush(companion, reason)
}

pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        voice.client_moved(companion, client);
    }
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        let runtime = app.world().resource::<HostRuntime>().0.clone();
        app.init_resource::<delegation::DeliveredClaims>()
            .insert_resource(Voice::new(runtime));
    }
}

mod plugin;
#[cfg(test)]
mod tests;
