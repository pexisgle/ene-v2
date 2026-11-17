//! Host-side voice pipeline: recognition input, sentence-level synthesis
//! output, playback ownership and the speech floor (docs/design/voice-avatar.md
//! §4–§5, docs/api.md §6–§7). Recognition runs in the Connect layer; synthesis
//! tasks run on the host runtime and release audio in `sequence_no` order.
use super::{
    agent,
    db::{self, Result, Store},
    delegation,
    error::HostError,
    model::Credentials,
    notify::Notifier,
    permission::{self, ApprovalDecision, ApprovalStatus},
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
    time::{Duration, Instant},
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
const PLAYBACK_GRACE: Duration = Duration::from_secs(3);
const TRANSCRIPTION_MEMORY: usize = 128;
const MAX_SENTENCES: u32 = 64;
const AUDIO_QUEUE: usize = 32;
/// Completed companion utterances the host relays to the peers without a new
/// user input before it stops the autonomous chain (voice-avatar.md §5.3).
const MAX_RELAY: u32 = 5;

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

pub(crate) fn configured(
    world: &World,
    kind: ConnectionKind,
) -> std::result::Result<(), SetupError> {
    voice_connection(world, kind).map(|_| ())
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
    chars: u32,
    finished: bool,
    voice: String,
    source: Arc<dyn TtsEngine>,
    buffer: String,
    total_sentences: u32,
    deadline: Option<Instant>,
}

#[derive(Clone, Default)]
struct Owner {
    client_id: String,
    activation_revision: u64,
    generation_id: u64,
    utterance: Option<Utterance>,
}

struct Work {
    companion: String,
    generation_id: u64,
    emitter: Emitter,
    pending: VecDeque<Sentence>,
    notify: Arc<Notify>,
    task: Option<AbortOnDropHandle<()>>,
}

impl Work {
    fn new(companion: &str, utterance_id: &str, generation_id: u64) -> Self {
        Self {
            companion: companion.to_owned(),
            generation_id,
            emitter: Emitter::new(utterance_id.to_owned(), generation_id),
            pending: VecDeque::new(),
            notify: Arc::new(Notify::new()),
            task: None,
        }
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
    /// Completed companion utterances broadcast since the last user input.
    relay: u32,
}

impl State {
    fn cancel(&mut self, utterance_id: &str) {
        self.works.remove(utterance_id);
    }
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

    fn publish(&self, companion: &str, client: &str, revision: u64) {
        let mut state = self.lock();
        let owner = state.owners.entry(companion.to_owned()).or_default();
        owner.client_id = client.to_owned();
        owner.activation_revision = revision;
    }

    fn owner_of(&self, utterance_id: &str) -> Option<(String, Owner)> {
        self.lock().owners.iter().find_map(|(companion, owner)| {
            owner
                .utterance
                .as_ref()
                .is_some_and(|utterance| utterance.id == utterance_id)
                .then(|| (companion.clone(), owner.clone()))
        })
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
            chars: 0,
            finished: false,
            voice,
            source,
            buffer: String::new(),
            total_sentences: 0,
            deadline: None,
        });
        state
            .works
            .insert(id.clone(), Work::new(companion, &id, generation_id));
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
                utterance.chars += text.chars().count() as u32;
                jobs.push(sentence(utterance, sequence_no, text));
            }
        }
        if overflow {
            finish(&mut state, companion, Some(w::FlushReason::Cancelled));
            return;
        }
        if let Some(work) = state.works.get_mut(&utterance_id) {
            work.pending.extend(jobs);
        }
        self.pump(&mut state, &utterance_id);
    }

    fn end_turn(&self, companion: &str) {
        let mut state = self.lock();
        if state.floor.as_deref() != Some(companion) {
            return;
        }
        let Some(utterance_id) = state
            .owners
            .get(companion)
            .and_then(|owner| owner.utterance.as_ref())
            .map(|utterance| utterance.id.clone())
        else {
            finish(&mut state, companion, None);
            return;
        };
        let mut jobs = Vec::new();
        let mut overflow = false;
        let total;
        {
            let Some(utterance) = state
                .owners
                .get_mut(companion)
                .and_then(|owner| owner.utterance.as_mut())
            else {
                return;
            };
            if utterance.finished {
                return;
            }
            if let Some(text) = flush_sentences(&mut utterance.buffer) {
                if utterance.next_sequence >= MAX_SENTENCES {
                    overflow = true;
                } else {
                    let sequence_no = utterance.next_sequence;
                    utterance.next_sequence += 1;
                    utterance.chars += text.chars().count() as u32;
                    jobs.push(sentence(utterance, sequence_no, text));
                }
            }
            total = utterance.next_sequence;
            if !overflow {
                utterance.finished = true;
                if total > 0 {
                    utterance.total_sentences = total;
                    let speech =
                        Duration::from_secs_f64((f64::from(utterance.chars) * 0.18).min(600.0));
                    utterance.deadline = Some(Instant::now() + speech + PLAYBACK_GRACE);
                }
            }
        }
        if overflow {
            finish(&mut state, companion, Some(w::FlushReason::Cancelled));
            return;
        }
        if total == 0 {
            finish(&mut state, companion, None);
            return;
        }
        let Some(work) = state.works.get_mut(&utterance_id) else {
            return;
        };
        work.pending.extend(jobs);
        let generation_id = work.generation_id;
        let events = work.emitter.finish(total);
        self.send(&mut state, companion, generation_id, events, &utterance_id);
        self.pump(&mut state, &utterance_id);
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

    fn client_moved(&self, companion: &str, client: &str, revision: u64) {
        let mut state = self.lock();
        let owns = state.owners.get(companion).is_some_and(|owner| {
            owner.client_id == client && owner.activation_revision == revision
        });
        if !owns {
            return;
        }
        finish(&mut state, companion, Some(w::FlushReason::ClientMoved));
        state.owners.remove(companion);
    }

    fn release(&self, companion: &str) {
        let mut state = self.lock();
        finish(&mut state, companion, None);
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
                {
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
                    if pending.is_empty() {
                        work.task.take();
                        return;
                    }
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
        let Some(work) = state.works.get_mut(utterance_id) else {
            return;
        };
        let events = work.emitter.offer(sequence_no, outcome);
        let companion = work.companion.clone();
        let generation_id = work.generation_id;
        let finished = work.emitter.sent_finished;
        self.send(&mut state, &companion, generation_id, events, utterance_id);
        if finished {
            state.cancel(utterance_id);
        }
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

    /// Counts one more relayed companion utterance. After [`MAX_RELAY`] the
    /// autonomous chain pauses: the completed speech still releases the floor
    /// and the head still answers an explicit user turn, but no
    /// `speech_completed` event is broadcast, so no peer is prompted to answer
    /// itself again (voice-avatar.md §5.3, the cap is on user-input-free
    /// relay turns).
    fn relay_open(&self) -> bool {
        let mut state = self.lock();
        if state.relay >= MAX_RELAY {
            return false;
        }
        state.relay += 1;
        true
    }

    /// An accepted user input opens a new relay budget.
    fn note_user_input(&self) {
        self.lock().relay = 0;
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

struct Emitter {
    utterance_id: String,
    generation_id: u64,
    next: u32,
    finished: Option<u32>,
    sent_finished: bool,
}

impl Emitter {
    fn new(utterance_id: String, generation_id: u64) -> Self {
        Self {
            utterance_id,
            generation_id,
            next: 0,
            finished: None,
            sent_finished: false,
        }
    }

    fn offer(
        &mut self,
        sequence_no: u32,
        outcome: HttpResult<Vec<u8>>,
    ) -> Vec<w::AudioOutputEvent> {
        if self.sent_finished {
            return Vec::new();
        }
        let mut emitted = Vec::new();
        self.next += 1;
        match outcome {
            Ok(pcm) => self.emit_audio(sequence_no, pcm, &mut emitted),
            Err(reason) => emitted.push(skipped_event(
                &self.utterance_id,
                self.generation_id,
                sequence_no,
                reason,
            )),
        }
        self.emit_finished(&mut emitted);
        emitted
    }

    fn finish(&mut self, total_sentences: u32) -> Vec<w::AudioOutputEvent> {
        self.finished = Some(total_sentences);
        let mut emitted = Vec::new();
        self.emit_finished(&mut emitted);
        emitted
    }

    fn emit_audio(&self, sequence_no: u32, pcm: Vec<u8>, emitted: &mut Vec<w::AudioOutputEvent>) {
        if pcm.is_empty() {
            return;
        }
        let chunks = pcm.chunks(AUDIO_CHUNK_BYTES);
        let total = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            emitted.push(audio_event(w::AudioChunkEnvelope {
                utterance_id: self.utterance_id.clone(),
                generation_id: self.generation_id,
                sequence_no,
                pcm_data: chunk.to_vec(),
                sample_rate: SAMPLE_RATE,
                is_final_chunk: index + 1 == total,
                ..Default::default()
            }));
        }
    }

    fn emit_finished(&mut self, emitted: &mut Vec<w::AudioOutputEvent>) {
        let Some(total_sentences) = self.finished else {
            return;
        };
        if self.sent_finished || self.next < total_sentences {
            return;
        }
        self.sent_finished = true;
        emitted.push(audio_event(w::UtteranceFinished {
            utterance_id: self.utterance_id.clone(),
            generation_id: self.generation_id,
            total_sentences,
            ..Default::default()
        }));
    }
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

fn skipped_event(
    utterance_id: &str,
    generation_id: u64,
    sequence_no: u32,
    reason: impl Into<String>,
) -> w::AudioOutputEvent {
    audio_event(w::SentenceSkipped {
        utterance_id: utterance_id.to_owned(),
        generation_id,
        sequence_no,
        reason: reason.into(),
        ..Default::default()
    })
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
    pub(crate) activation_revision: u64,
}

impl InputBinding {
    pub(crate) fn accepts(&self, client_id: &str, activation_revision: u64) -> bool {
        (client_id.is_empty() || client_id == self.client_id)
            && (activation_revision == 0 || activation_revision == self.activation_revision)
    }
}

pub(crate) fn bind_input(
    world: &World,
    client_id: &str,
    activation_revision: u64,
) -> Result<InputBinding> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(
            ("client_id is required for audio input").into(),
        ));
    }
    db::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>() else {
        return Err(HostError::Unavailable(
            ("the host state is not available").into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let activations = store.activations()?;
    let active = activations
        .iter()
        .any(|activation| activation.client_id.as_deref() == Some(client_id));
    let matched = activations.iter().any(|activation| {
        activation.client_id.as_deref() == Some(client_id)
            && activation.revision == activation_revision
    });
    if !active {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    if !matched {
        return Err(HostError::Conflict(
            ("client_moved: the activation revision is stale").into(),
        ));
    }
    Ok(InputBinding {
        client_id: client_id.to_owned(),
        activation_revision,
    })
}

pub(crate) fn is_still_active(world: &World, binding: &InputBinding) -> bool {
    let Some(store) = world.get_resource::<Store>() else {
        return false;
    };
    store.activations().is_ok_and(|activations| {
        activations.iter().any(|activation| {
            activation.client_id.as_deref() == Some(binding.client_id.as_str())
                && activation.revision == binding.activation_revision
        })
    })
}

/// The approval a speech capture is bound to at its start
/// (docs/design/permissions.md §4.2). The final transcript may answer only
/// this request, and only while it is still pending for the same client
/// generation; a delayed STT result must never apply to a later request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpeechBinding {
    /// The `approval_requests.request_id` captured when the capture started.
    pub(crate) speech_start_request_id: String,
    pub(crate) client_id: String,
    /// The activation revision the approval was bound to.
    pub(crate) generation: u64,
}

/// Binds the capture to the one pending approval of this client generation.
/// Two or more pending approvals are ambiguous, so voice stays out and the
/// screen card decides (permissions.md §4.2 "同時に1件のみ").
pub(crate) fn bind_speech_start(world: &World, binding: &InputBinding) -> Option<SpeechBinding> {
    let store = world.get_resource::<Store>()?;
    let pending = store.approvals(ApprovalStatus::Pending).ok()?;
    let mut candidates = pending.into_iter().filter(|approval| {
        approval.client_id.as_deref() == Some(binding.client_id.as_str())
            && approval.activation_revision == Some(binding.activation_revision)
    });
    let approval = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(SpeechBinding {
        speech_start_request_id: approval.request_id,
        client_id: binding.client_id.clone(),
        generation: binding.activation_revision,
    })
}

const AFFIRMATIVE_JAPANESE: [&str; 6] =
    ["はい", "いいよ", "いいです", "お願い", "実行して", "どうぞ"];
const NEGATIVE_JAPANESE: [&str; 5] = ["いいえ", "だめ", "やめて", "やらない", "キャンセル"];
const AFFIRMATIVE_ENGLISH: [&str; 4] = ["yes", "approve", "approved", "ok"];
const NEGATIVE_ENGLISH: [&str; 4] = ["no", "deny", "denied", "cancel"];

/// A conservative keyword intent for a voice approval (permissions.md §4.2).
///
/// This is an approximation, not natural-language understanding: a Japanese
/// word matches as a substring, an English word must match a whole token (so
/// "no" does not fire inside "not"), and a negative word wins when both kinds
/// appear. Anything else is discarded, because an ambiguous utterance must not
/// approve an irreversible operation.
fn approval_intent(text: &str) -> Option<ApprovalDecision> {
    let lower = text.trim().to_lowercase();
    let negative = NEGATIVE_JAPANESE.iter().any(|word| lower.contains(word))
        || english_tokens(&lower).any(|word| NEGATIVE_ENGLISH.contains(&word));
    if negative {
        return Some(ApprovalDecision::Deny);
    }
    let affirmative = AFFIRMATIVE_JAPANESE.iter().any(|word| lower.contains(word))
        || english_tokens(&lower).any(|word| AFFIRMATIVE_ENGLISH.contains(&word));
    affirmative.then_some(ApprovalDecision::Approve)
}

fn english_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
}

/// Answers the approval bound when the capture started, if the final
/// transcript carries an unambiguous intent and the request survived until
/// now. A stale request, a moved generation or a non-matching binding is
/// discarded; the transcript itself was already recorded as an ordinary
/// utterance.
fn submit_voice_approval(
    world: &mut World,
    text: &str,
    binding: &InputBinding,
    speech: &SpeechBinding,
) {
    let Some(decision) = approval_intent(text) else {
        return;
    };
    if speech.client_id != binding.client_id || speech.generation != binding.activation_revision {
        return;
    }
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return;
    };
    let Ok(Some(approval)) = store.approval(&speech.speech_start_request_id) else {
        return;
    };
    // A screen click, a stop or a client move may have resolved the request
    // while the STT ran: the delayed text must not answer anything else.
    if approval.status != ApprovalStatus::Pending
        || approval.client_id.as_deref() != Some(binding.client_id.as_str())
        || approval.activation_revision != Some(binding.activation_revision)
        || !is_still_active(world, binding)
    {
        return;
    }
    match permission::submit(world, &speech.speech_start_request_id, decision) {
        Ok(approval) => tracing::debug!(
            request = %approval.request_id,
            status = approval.status.as_str(),
            "a voice transcript answered an approval"
        ),
        Err(error) => tracing::debug!(
            request = %speech.speech_start_request_id,
            %error,
            "a voice approval was discarded"
        ),
    }
}

pub(crate) struct AudioSubscription {
    pub(crate) subscription_id: String,
    pub(crate) receiver: mpsc::Receiver<w::AudioOutputEvent>,
    pub(crate) control: mpsc::UnboundedReceiver<w::AudioOutputEvent>,
}

pub(crate) fn subscribe_audio(
    world: &mut World,
    client_id: &str,
    activation_revision: u64,
) -> Result<AudioSubscription> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(("client_id is required").into()));
    }
    db::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Err(HostError::Unavailable(
            ("the host state is not available").into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let activations = store.activations()?;
    if !activations
        .iter()
        .any(|activation| activation.client_id.as_deref() == Some(client_id))
    {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    let owned: Vec<String> = activations
        .iter()
        .filter(|activation| {
            activation.client_id.as_deref() == Some(client_id)
                && activation.revision == activation_revision
        })
        .map(|activation| activation.companion_id.clone())
        .collect();
    if owned.is_empty() {
        return Err(HostError::Conflict(
            ("client_moved: the activation revision is stale").into(),
        ));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::Internal(
            ("the voice plugin is not installed").into(),
        ));
    };
    let subscription = voice.register(client_id);
    for companion_id in owned {
        voice.publish(&companion_id, client_id, activation_revision);
    }
    Ok(subscription)
}

pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.unregister(client, subscription);
    }
}

pub(crate) fn report_playback(
    world: &mut World,
    report: w::PlaybackStatusReport,
) -> Result<w::ReportPlaybackStatusResponse> {
    let response = |acknowledged| w::ReportPlaybackStatusResponse {
        acknowledged,
        ..Default::default()
    };
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(response(false));
    };
    db::validate_id(&report.client_id)?;
    if store.client(&report.client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Ok(response(false));
    };
    let Some((companion, owner)) = voice.owner_of(&report.utterance_id) else {
        return Ok(response(false));
    };
    if owner.client_id != report.client_id
        || owner.activation_revision != report.activation_revision
        || owner.generation_id != report.generation_id
    {
        return Ok(response(false));
    }
    let current = store
        .activation(&companion)?
        .map_or(0, |activation| activation.revision);
    if current != owner.activation_revision {
        return Ok(response(false));
    }
    let status = report.status.as_known();
    let completed = status == Some(w::playback_status_report::Status::PLAYBACK_COMPLETED);
    let ended = completed
        || matches!(
            status,
            Some(
                w::playback_status_report::Status::PLAYBACK_INTERRUPTED
                    | w::playback_status_report::Status::PLAYBACK_FAILED
            )
        );
    if !ended {
        return Ok(response(false));
    }
    // The report passed the generation, client and activation checks; a stale
    // position was refused above and never reaches the floor release.
    let total_sentences = owner
        .utterance
        .as_ref()
        .map(|utterance| utterance.total_sentences);
    voice.release(&companion);
    if completed && voice.relay_open() {
        broadcast_speech_completed(world, &companion, &report, total_sentences);
    }
    Ok(response(true))
}

fn broadcast_speech_completed(
    world: &mut World,
    companion: &str,
    report: &w::PlaybackStatusReport,
    total_sentences: Option<u32>,
) {
    let document = w::JsonDocument {
        schema: "ene.speech_completed.v1".into(),
        json: serde_json::json!({
            "companion_id": companion, "client_id": report.client_id,
            "utterance_id": report.utterance_id, "generation_id": report.generation_id,
            "total_sentences": total_sentences, "completed_sentence_count": report.completed_sentence_count,
            "completed_pcm_bytes": report.completed_pcm_bytes, "played_sequence_nos": report.played_sequence_nos,
            "skipped_sequence_nos": report.skipped_sequence_nos,
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

pub(crate) fn accept_transcription(
    world: &mut World,
    transcription_id: &str,
    text: &str,
    binding: &InputBinding,
    speech: Option<&SpeechBinding>,
) -> Result<bool> {
    let text = text.trim();
    if text.is_empty() || !is_still_active(world, binding) {
        return Ok(false);
    }
    if let Some(voice) = world.get_resource::<Voice>() {
        if !voice.accept_transcription(transcription_id) {
            return Ok(false);
        }
        // A new user turn ends the autonomous relay chain (§5.3).
        voice.note_user_input();
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
    // The transcript is recorded above whatever the approval outcome is: an
    // ambiguous or stale voice answer never stops the utterance from reaching
    // the companions.
    if let Some(speech) = speech {
        submit_voice_approval(world, text, binding, speech);
    }
    Ok(true)
}

/// An accepted chat message opens a new autonomous relay budget, exactly like
/// an accepted transcription.
pub(crate) fn note_user_input(world: &World) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.note_user_input();
    }
}

pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    if !voice.owns(companion) {
        return;
    }
    if !voice.streaming(companion) {
        let Ok(source) = tts_source(world) else {
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
            return;
        }
    }
    voice.speak(companion, text);
}

pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.end_turn(companion);
    }
}

pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::NotFound(
            ("no client owns this companion's audio output").into(),
        ));
    };
    voice.flush(companion, reason)
}

pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str, revision: u64) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        voice.client_moved(companion, client, revision);
    }
}

fn expire_playback(world: &mut World) {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    let now = Instant::now();
    let expired: Vec<String> = {
        let state = voice.lock();
        state
            .owners
            .iter()
            .filter(|(_, owner)| {
                owner
                    .utterance
                    .as_ref()
                    .and_then(|utterance| utterance.deadline)
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(companion, _)| companion.clone())
            .collect()
    };
    for companion in expired {
        tracing::debug!(%companion, "playback did not report; releasing the speech floor");
        voice.release(&companion);
    }
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        let runtime = app.world().resource::<HostRuntime>().0.clone();
        app.init_resource::<delegation::DeliveredClaims>()
            .insert_resource(Voice::new(runtime))
            .add_systems(Update, expire_playback);
    }
}

mod plugin;
#[cfg(test)]
mod tests;
