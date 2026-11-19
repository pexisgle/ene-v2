//! Playback and capture on the client's own devices.
//!
//! The wire format is fixed by the host: 24 kHz (or 16 kHz) 16-bit signed
//! little-endian mono PCM (`apps/ene-host/src/host/voice.rs`). The device format
//! is whatever the device reports, so this module queries it and either opens at
//! the wire rate directly or converts with `rubato`.
//!
//! Two rules shape the code below.
//!
//! - A continuous stream keeps one resampler for its whole life. Rebuilding one
//!   per chunk restarts the filter and the audio clicks; the `Filter` type here
//!   owns the `rubato` state and accumulates into the fixed chunk size the
//!   library requires.
//! - The audio callback does no I/O, no RPC, no allocation and no blocking lock.
//!   It pops samples, counts them, and marks sentences as they finish. Capture
//!   pushes device samples into a pre-filled ring with `try_lock` and lets the
//!   session thread do the conversion.
//!
//! Only one utterance is audible at a time, so a new utterance or a flush stops
//! the old one before anything else runs.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

/// The wire format the host and the client agree on.
pub const WIRE_RATE: u32 = 24_000;

/// The chunk the resampler converts at a time. Small enough to keep latency
/// under a frame budget, large enough for the FFT filter to be efficient.
const CHUNK: usize = 480;

/// A streaming sample-rate converter that keeps its state across chunks.
struct Filter {
    from: u32,
    to: u32,
    inner: rubato::Fft<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Filter {
    fn new(from: u32, to: u32) -> Result<Self, String> {
        Self::with_chunk(from, to, CHUNK)
    }

    fn with_chunk(from: u32, to: u32, chunk: usize) -> Result<Self, String> {
        let inner = rubato::Fft::<f32>::new(
            from as usize,
            to as usize,
            chunk,
            1,
            rubato::FixedSync::Input,
        )
        .map_err(|error| format!("the sample rate cannot be converted: {error}"))?;
        Ok(Self {
            from,
            to,
            inner,
            pending: Vec::with_capacity(CHUNK * 2),
            output: vec![0.; CHUNK * 4],
        })
    }

    fn reset(&mut self) {
        use rubato::Resampler;
        self.inner.reset();
        self.pending.clear();
    }

    fn finish(&mut self) -> Result<Vec<f32>, String> {
        if self.pending.is_empty() {
            self.reset();
            return Ok(Vec::new());
        }
        use rubato::Resampler;
        let remainder = self.pending.len();
        let want = self.inner.input_frames_next();
        let pad_zeroes = want.saturating_sub(remainder % want);
        if pad_zeroes > 0 && !remainder.is_multiple_of(want) {
            self.pending.resize(remainder + pad_zeroes, 0.0);
        }
        let mut out = self.push(&[])?;
        let delay = self.inner.output_delay();
        let expected_tail =
            ((remainder as f64) * (self.to as f64) / (self.from as f64)).ceil() as usize;
        if delay > 0 && out.len() < expected_tail {
            let flush_pad = vec![0.0; want];
            out.extend(self.push(&flush_pad)?);
        }
        if out.len() > expected_tail {
            out.truncate(expected_tail);
        }
        self.reset();
        Ok(out)
    }

    /// Converts everything that completes a chunk, keeping the remainder.
    fn push(&mut self, input: &[f32]) -> Result<Vec<f32>, String> {
        use rubato::Resampler;
        self.pending.extend_from_slice(input);
        let mut out = Vec::new();
        loop {
            let want = self.inner.input_frames_next();
            if self.pending.len() < want {
                break;
            }
            let source = rubato::audioadapter_buffers::direct::InterleavedSlice::new(
                &self.pending[..want],
                1,
                want,
            )
            .map_err(|error| error.to_string())?;
            let frames = self.inner.output_frames_max();
            if self.output.len() < frames {
                self.output.resize(frames, 0.);
            }
            let mut sink = rubato::audioadapter_buffers::direct::InterleavedSlice::new_mut(
                &mut self.output[..frames],
                1,
                frames,
            )
            .map_err(|error| error.to_string())?;
            let (consumed, produced) = self
                .inner
                .process_into_buffer(&source, &mut sink, None)
                .map_err(|error| format!("the audio was not converted: {error}"))?;
            out.extend_from_slice(&self.output[..produced]);
            self.pending.drain(..consumed);
        }
        Ok(out)
    }
}

/// What the mixer knows about the utterance being played.
#[derive(Default)]
struct Deck {
    /// The generation currently allowed to make sound.
    generation: u64,
    utterance: String,
    /// Decoded, resampled, device-rate mono samples waiting to be played.
    pending: VecDeque<f32>,
}

struct PlayerQueue {
    deck: Arc<Mutex<Deck>>,
    filter: Option<Filter>,
    flushed: Arc<AtomicU64>,
}

impl PlayerQueue {
    fn new(device_rate: u32) -> Result<Self, String> {
        Ok(Self {
            deck: Arc::new(Mutex::new(Deck::default())),
            filter: (device_rate != WIRE_RATE)
                .then(|| Filter::new(WIRE_RATE, device_rate))
                .transpose()?,
            flushed: Arc::new(AtomicU64::new(0)),
        })
    }

    fn push(&mut self, utterance: &str, generation: u64, pcm: &[u8]) -> Result<(), String> {
        if generation == 0 || generation <= self.flushed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let decoded = decode(pcm)?;
        let switched = {
            let deck = self
                .deck
                .lock()
                .map_err(|_| "the audio deck was poisoned")?;
            if generation < deck.generation {
                return Ok(());
            }
            deck.generation != generation || deck.utterance != utterance
        };
        if switched && let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        let samples = match self.filter.as_mut() {
            Some(filter) => filter.push(&decoded)?,
            None => decoded,
        };
        let mut deck = self
            .deck
            .lock()
            .map_err(|_| "the audio deck was poisoned")?;
        if generation <= self.flushed.load(Ordering::Relaxed) || generation < deck.generation {
            return Ok(());
        }
        if deck.generation != generation || deck.utterance != utterance {
            deck.pending.clear();
            deck.generation = generation;
            deck.utterance = utterance.to_owned();
        }
        deck.pending.extend(samples);
        Ok(())
    }

    fn finish_utterance(&mut self, utterance: &str, generation: u64) -> Result<(), String> {
        if generation == 0 || generation <= self.flushed.load(Ordering::Relaxed) {
            if let Some(filter) = self.filter.as_mut() {
                filter.reset();
            }
            return Ok(());
        }
        let active = {
            let deck = self
                .deck
                .lock()
                .map_err(|_| "the audio deck was poisoned")?;
            deck.generation == generation && deck.utterance == utterance
        };
        if !active {
            return Ok(());
        }
        let tail = match self.filter.as_mut() {
            Some(filter) => filter.finish()?,
            None => Vec::new(),
        };
        if tail.is_empty() {
            return Ok(());
        }
        let mut deck = self
            .deck
            .lock()
            .map_err(|_| "the audio deck was poisoned")?;
        if generation > self.flushed.load(Ordering::Relaxed)
            && deck.generation == generation
            && deck.utterance == utterance
        {
            deck.pending.extend(tail);
        }
        Ok(())
    }

    fn stop_current(&mut self) {
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        if let Ok(mut deck) = self.deck.lock() {
            deck.pending.clear();
            deck.utterance.clear();
        }
    }

    fn flush(&mut self, generation: u64) {
        self.flushed.fetch_max(generation, Ordering::Relaxed);
        if let Ok(mut deck) = self.deck.lock()
            && deck.generation <= generation
        {
            deck.pending.clear();
            deck.utterance.clear();
            if let Some(filter) = self.filter.as_mut() {
                filter.reset();
            }
        }
    }

    fn reset_stream(&mut self) {
        self.flushed.store(0, Ordering::Relaxed);
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        if let Ok(mut deck) = self.deck.lock() {
            deck.generation = 0;
            deck.pending.clear();
            deck.utterance.clear();
        }
    }
}

/// The device-side player. One client owns one deck.
pub struct Player {
    queue: PlayerQueue,
    rate: u32,
    channels: usize,
    /// The stream must stay alive for audio to continue; dropping it stops it.
    _stream: cpal::Stream,
    /// RMS of the buffer the device last played, for lip sync.
    level: Arc<AtomicU32>,
    /// Software mute: drains the deck without audible output.
    muted: Arc<AtomicBool>,
}

impl Player {
    /// Opens the default output device, preferring a configuration that needs no
    /// conversion.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("the machine has no default output device")?;
        let supported = pick_output(&device)?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let queue = PlayerQueue::new(rate)?;
        let level = Arc::new(AtomicU32::new(0));
        let muted = Arc::new(AtomicBool::new(false));
        let sink = queue.deck.clone();
        let rms = level.clone();
        let out_muted = muted.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                supported.config(),
                move |output: &mut [f32], _| {
                    fill_with(output, channels, &sink, &rms, &out_muted, 0.0f32, |s| s);
                },
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                supported.config(),
                move |output: &mut [i16], _| {
                    fill_with(output, channels, &sink, &rms, &out_muted, 0i16, f32_to_i16);
                },
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                supported.config(),
                move |output: &mut [u16], _| {
                    fill_with(
                        output,
                        channels,
                        &sink,
                        &rms,
                        &out_muted,
                        U16_SILENCE,
                        f32_to_u16,
                    );
                },
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            other => {
                return Err(format!(
                    "the output device uses {other} samples, which this client does not convert"
                ));
            }
        }
        .map_err(|error| format!("the output stream could not start: {error}"))?;
        stream
            .play()
            .map_err(|error| format!("the output stream could not start: {error}"))?;
        Ok(Self {
            queue,
            rate,
            channels,
            _stream: stream,
            level,
            muted,
        })
    }

    /// Mutes or unmutes local speaker output without stalling host playback tracking.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.level.store(0f32.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// The mouth amplitude the device last played, in 0..=1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Queues one wire chunk. A chunk for a flushed or superseded generation is
    /// dropped, which is what makes a late arrival harmless.
    pub fn push(&mut self, utterance: &str, generation: u64, pcm: &[u8]) -> Result<(), String> {
        self.queue.push(utterance, generation, pcm)
    }

    /// Drains any buffered resampler tail at utterance completion.
    pub fn finish_utterance(&mut self, utterance: &str, generation: u64) -> Result<(), String> {
        self.queue.finish_utterance(utterance, generation)
    }

    /// Stops only what is currently buffered without poisoning future generations.
    pub fn stop_current(&mut self) {
        self.queue.stop_current();
    }

    /// Stops everything at or below `generation`.
    pub fn flush(&mut self, generation: u64) {
        self.queue.flush(generation);
    }

    /// Resets generation tracking when opening a fresh audio subscription stream.
    pub fn reset_stream(&mut self) {
        self.queue.reset_stream();
    }

    /// Whether audio is still queued for the current utterance.
    pub fn playing(&self) -> bool {
        self.queue
            .deck
            .lock()
            .map(|deck| !deck.pending.is_empty())
            .unwrap_or(false)
    }

    /// The device rate the wire audio is played at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

const U16_SILENCE: u16 = 32_768;

fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1., 1.) * 32_767.).round() as i16
}

fn f32_to_u16(sample: f32) -> u16 {
    ((sample.clamp(-1., 1.) * 32_767.).round() + 32_768.) as u16
}

fn i16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / 32_768.0
}

fn u16_to_f32(sample: u16) -> f32 {
    (f32::from(sample) - 32_768.0) / 32_768.0
}

/// Fills one device buffer from the deck, duplicating mono into every channel.
fn fill_with<T: Copy>(
    output: &mut [T],
    channels: usize,
    deck: &Arc<Mutex<Deck>>,
    level: &AtomicU32,
    muted: &AtomicBool,
    silence: T,
    to_device: impl Fn(f32) -> T,
) {
    if channels == 0 {
        output.fill(silence);
        level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    }
    let Ok(mut deck) = deck.try_lock() else {
        // The session thread is queueing; silence one buffer rather than block.
        output.fill(silence);
        level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    };
    let is_muted = muted.load(Ordering::Relaxed);
    let mut energy = 0f64;
    let mut audible = 0u64;
    for frame in output.chunks_mut(channels) {
        let sample = deck.pending.pop_front().unwrap_or(0.);
        energy += f64::from(sample) * f64::from(sample);
        if sample != 0. {
            audible += 1;
        }
        let written = if is_muted { silence } else { to_device(sample) };
        for slot in frame.iter_mut() {
            *slot = written;
        }
    }
    // The RMS of what this buffer actually sent is the mouth opening. Silence
    // when nothing played or when muted, so a drained or muted deck closes the mouth.
    let rms = if is_muted || audible == 0 {
        0.
    } else {
        (energy / audible as f64).sqrt() as f32
    };
    level.store(rms.to_bits(), Ordering::Relaxed);
}

#[cfg(test)]
fn fill(
    output: &mut [f32],
    channels: usize,
    deck: &Arc<Mutex<Deck>>,
    level: &AtomicU32,
    muted: &AtomicBool,
) {
    fill_with(output, channels, deck, level, muted, 0.0f32, |s| s);
}

/// Decodes the wire's 16-bit signed little-endian mono PCM.
pub fn decode(pcm: &[u8]) -> Result<Vec<f32>, String> {
    if !pcm.len().is_multiple_of(2) {
        return Err("the host sent an odd number of PCM bytes".into());
    }
    Ok(pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect())
}

/// Encodes device samples as the wire's 16-bit signed little-endian mono PCM.
pub fn encode(samples: &[f32]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let value = (sample.clamp(-1., 1.) * 32_767.).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    pcm
}

fn choose_rate(min_rate: u32, max_rate: u32) -> u32 {
    let (lo, hi) = (min_rate.min(max_rate), min_rate.max(max_rate));
    if (lo..=hi).contains(&WIRE_RATE) {
        WIRE_RATE
    } else if (lo..=hi).contains(&48_000) {
        48_000
    } else if (lo..=hi).contains(&44_100) {
        44_100
    } else {
        48_000.clamp(lo, hi)
    }
}

fn format_score(format: cpal::SampleFormat) -> u8 {
    match format {
        cpal::SampleFormat::F32 => 3,
        cpal::SampleFormat::I16 => 2,
        cpal::SampleFormat::U16 => 1,
        _ => 0,
    }
}

fn score_candidate(
    channels: u16,
    min_rate: u32,
    max_rate: u32,
    format: cpal::SampleFormat,
) -> (u8, u8, u8, u32) {
    let rate = choose_rate(min_rate, max_rate);
    let fmt = format_score(format);
    let ch = match channels {
        0 => 0,
        1 => 3,
        2 => 2,
        _ => 1,
    };
    if fmt == 0 || ch == 0 {
        return (0, 0, 0, rate);
    }
    let rate_score = if rate == WIRE_RATE {
        2
    } else if rate == 48_000 || rate == 44_100 {
        1
    } else {
        0
    };
    (fmt, rate_score, ch, rate)
}

/// Picks the device configuration closest to the wire format (`24 kHz` mono),
/// preferring `F32` over `I16`/`U16` and avoiding `min_sample_rate()` on
/// continuous ALSA/PipeWire ranges (`1..=384000 Hz`).
fn pick_config_from_ranges(
    ranges: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
    direction: &str,
) -> Result<Option<cpal::SupportedStreamConfig>, String> {
    let mut best: Option<((u8, u8, u8), cpal::SupportedStreamConfig)> = None;
    let mut rejected: Option<(cpal::SampleFormat, u32)> = None;
    for range in ranges {
        let (fmt, rate_score, ch, rate) = score_candidate(
            range.channels(),
            range.min_sample_rate(),
            range.max_sample_rate(),
            range.sample_format(),
        );
        if fmt == 0 || ch == 0 {
            rejected.get_or_insert((range.sample_format(), rate));
            continue;
        }
        let score = (fmt, rate_score, ch);
        if best.as_ref().is_none_or(|(current, _)| score > *current) {
            best = Some((score, range.with_sample_rate(rate)));
        }
    }
    if let Some((_, config)) = best {
        return Ok(Some(config));
    }
    if let Some((fmt, rate)) = rejected {
        return Err(format!(
            "the {direction} device offers {fmt} at {rate} Hz, and this client converts only 32-bit float or 16-bit PCM"
        ));
    }
    Ok(None)
}

fn validate_default_config(
    config: cpal::SupportedStreamConfig,
    direction: &str,
) -> Result<cpal::SupportedStreamConfig, String> {
    if format_score(config.sample_format()) == 0 {
        return Err(format!(
            "the {direction} device uses {} samples, which this client does not convert",
            config.sample_format()
        ));
    }
    if config.channels() == 0 {
        return Err(format!("the {direction} device reports zero channels"));
    }
    Ok(config)
}

fn pick_output(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    if let Ok(ranges) = device.supported_output_configs()
        && let Some(config) = pick_config_from_ranges(ranges, "output")?
    {
        return Ok(config);
    }
    let fallback = device
        .default_output_config()
        .map_err(|error| format!("the output device has no default config: {error}"))?;
    validate_default_config(fallback, "output")
}

fn pick_input(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    if let Ok(ranges) = device.supported_input_configs()
        && let Some(config) = pick_config_from_ranges(ranges, "input")?
    {
        return Ok(config);
    }
    let fallback = device
        .default_input_config()
        .map_err(|error| format!("the input device has no default config: {error}"))?;
    validate_default_config(fallback, "input")
}

/// The capture side: microphone frames at the wire rate.
///
/// The callback only copies device samples into a bounded ring. Conversion to
/// the wire format happens in [`Microphone::take`], on the session thread.
const VAD_RATE: u32 = 16_000;
const VAD_FRAME_SAMPLES: usize = WIRE_RATE as usize * 16 / 1_000;
const VAD_START_FRAMES: u16 = 2; // 32 ms
const VAD_HANGOVER_FRAMES: u16 = 38; // 608 ms
const VAD_MAX_ACTIVE_FRAMES: u16 = 937; // about 15 s
const VAD_PRE_ROLL_SAMPLES: usize = VAD_FRAME_SAMPLES * 35; // 560 ms
const VAD_START_SCORE: f32 = 0.55;
const VAD_CONTINUE_SCORE: f32 = 0.35;

/// Microphone capture mode (`F07`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    /// Continuous voice-activity detection.
    #[default]
    Vad,
    /// Push-to-Talk: captures only while held, commits on release, discards on cancel.
    Ptt,
}

/// Far-end reference echo suppressor with barge-in detection (`F08`).
///
/// While the companion is speaking, speaker bleed into the microphone tracks the
/// far-end playback RMS. Genuine user speech interrupting the companion stays
/// meaningfully above `max(far_end_rms * ECHO_MARGIN, MIN_BARGE_IN_RMS)` for
/// `BARGE_IN_FRAMES` consecutive ticks before triggering a barge-in.
#[derive(Default)]
struct EchoSuppressor {
    over_threshold_frames: u8,
    barge_in_latched: bool,
}

const ECHO_MARGIN: f32 = 1.8;
const MIN_BARGE_IN_RMS: f32 = 0.06;
const BARGE_IN_FRAMES: u8 = 3;

impl EchoSuppressor {
    fn reset(&mut self) {
        self.over_threshold_frames = 0;
        self.barge_in_latched = false;
    }

    /// Evaluates one capture frame against far-end playback state.
    /// Returns `(allow_capture, barge_in_triggered)`.
    fn evaluate(&mut self, companion_speaking: bool, near_rms: f32, far_rms: f32) -> (bool, bool) {
        if !companion_speaking {
            self.reset();
            return (true, false);
        }
        if self.barge_in_latched {
            return (true, false);
        }
        let threshold = (far_rms * ECHO_MARGIN).max(MIN_BARGE_IN_RMS);
        if near_rms >= threshold {
            self.over_threshold_frames = self.over_threshold_frames.saturating_add(1);
            if self.over_threshold_frames >= BARGE_IN_FRAMES {
                self.barge_in_latched = true;
                return (true, true);
            }
        } else {
            self.over_threshold_frames = 0;
        }
        (false, false)
    }
}

/// Push-to-Talk capture state machine (`F07`).
#[derive(Default)]
struct PttController {
    active: bool,
    has_audio: bool,
    pending_commit: bool,
}

impl PttController {
    fn start(&mut self) {
        self.active = true;
        self.has_audio = false;
        self.pending_commit = false;
    }

    fn commit(&mut self) {
        if self.active {
            self.active = false;
            self.pending_commit = self.has_audio;
            self.has_audio = false;
        }
    }

    fn cancel(&mut self) {
        self.active = false;
        self.has_audio = false;
        self.pending_commit = false;
    }

    fn process(&mut self, samples: &[f32]) -> CaptureOutput {
        let mut chunks = Vec::new();
        if self.active {
            if !samples.is_empty() {
                chunks.push(encode(samples));
                self.has_audio = true;
            }
            return CaptureOutput {
                chunks,
                open: true,
                barge_in: false,
            };
        }
        if self.pending_commit {
            self.pending_commit = false;
            chunks.push(Vec::new());
        }
        CaptureOutput {
            chunks,
            open: false,
            barge_in: false,
        }
    }
}

#[derive(Default)]
pub struct CaptureOutput {
    /// Wire PCM in order. An empty chunk ends the preceding utterance.
    pub chunks: Vec<Vec<u8>>,
    pub open: bool,
    /// True on the tick user speech overrode companion playback (`F08`).
    pub barge_in: bool,
}

fn is_single_tone(frame: &[f32]) -> bool {
    // For a sinusoid, the mean of two equally spaced neighbors is a fixed
    // multiple of the center. Differencing the samples also removes DC.
    let (mut center_power, mut neighbor_power, mut product) = (0., 0., 0.);
    for samples in frame.windows(13) {
        let before = f64::from(samples[4] - samples[0]);
        let center = f64::from(samples[8] - samples[4]);
        let after = f64::from(samples[12] - samples[8]);
        let neighbors = (before + after) * 0.5;
        center_power += center * center;
        neighbor_power += neighbors * neighbors;
        product += center * neighbors;
    }
    center_power * neighbor_power - product * product <= 0.0001 * center_power * center_power
}

struct VoiceGate {
    detector: earshot::Detector,
    vad_filter: Filter,
    active: bool,
    speech_frames: u16,
    hangover: u16,
    active_frames: u16,
    pending: Vec<f32>,
    pre_roll: VecDeque<f32>,
}

impl VoiceGate {
    fn new() -> Result<Self, String> {
        Ok(Self {
            detector: earshot::Detector::default(),
            vad_filter: Filter::with_chunk(WIRE_RATE, VAD_RATE, VAD_FRAME_SAMPLES)?,
            active: false,
            speech_frames: 0,
            hangover: 0,
            active_frames: 0,
            pending: Vec::new(),
            pre_roll: VecDeque::with_capacity(VAD_PRE_ROLL_SAMPLES),
        })
    }

    fn reset(&mut self) -> bool {
        self.active = false;
        self.speech_frames = 0;
        self.hangover = 0;
        self.detector.reset();
        self.vad_filter.reset();
        self.pending.clear();
        self.pre_roll.clear();
        std::mem::take(&mut self.active_frames) > 0
    }

    fn push_pre_roll(&mut self, frame: &[f32]) {
        let excess = (self.pre_roll.len() + frame.len()).saturating_sub(VAD_PRE_ROLL_SAMPLES);
        self.pre_roll.drain(..excess);
        self.pre_roll.extend(frame);
    }

    fn process(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        self.pending.extend_from_slice(samples);
        let mut chunks = Vec::new();
        let mut audio = Vec::new();
        let mut offset = 0;
        while let Some(frame) = self.pending[offset..]
            .first_chunk::<VAD_FRAME_SAMPLES>()
            .copied()
        {
            offset += VAD_FRAME_SAMPLES;
            // Only the detector needs 16 kHz. Pre-roll and sent audio use the
            // original wire-rate samples so STT keeps their timing and spectrum.
            let mut vad_frame = self.vad_filter.push(&frame)?;
            for sample in &mut vad_frame {
                *sample = if sample.is_finite() {
                    sample.clamp(-1., 1.)
                } else {
                    0.
                };
            }
            let score = self.detector.predict_f32(&vad_frame);
            // Isolated hums and tones can still get high scores from the model.
            let score = if is_single_tone(&vad_frame) {
                0.
            } else {
                score
            };
            self.push_frame(&frame, score, &mut audio, &mut chunks);
        }
        self.pending.drain(..offset);
        if !audio.is_empty() {
            chunks.push(encode(&audio));
        }
        Ok(chunks)
    }

    fn push_frame(
        &mut self,
        frame: &[f32],
        score: f32,
        audio: &mut Vec<f32>,
        chunks: &mut Vec<Vec<u8>>,
    ) {
        if !self.active {
            self.push_pre_roll(frame);
            if score >= VAD_START_SCORE {
                self.speech_frames += 1;
            } else {
                self.speech_frames = 0;
            }
            if self.speech_frames == VAD_START_FRAMES {
                self.active = true;
                self.hangover = VAD_HANGOVER_FRAMES;
                self.active_frames = 1;
                audio.extend(self.pre_roll.drain(..));
            }
            return;
        }

        if score >= VAD_CONTINUE_SCORE {
            self.hangover = VAD_HANGOVER_FRAMES;
        } else if self.hangover == 0 {
            self.active = false;
            self.speech_frames = 0;
            self.push_pre_roll(frame);
            if std::mem::take(&mut self.active_frames) > 0 {
                Self::end_segment(audio, chunks);
            }
            return;
        } else {
            self.hangover -= 1;
        }

        audio.extend_from_slice(frame);
        self.active_frames += 1;
        if self.active_frames == VAD_MAX_ACTIVE_FRAMES {
            self.active_frames = 0;
            Self::end_segment(audio, chunks);
        }
    }

    fn end_segment(audio: &mut Vec<f32>, chunks: &mut Vec<Vec<u8>>) {
        if !audio.is_empty() {
            chunks.push(encode(audio));
            audio.clear();
        }
        chunks.push(Vec::new());
    }
}

pub struct Microphone {
    _stream: cpal::Stream,
    frames: Arc<Mutex<VecDeque<f32>>>,
    filter: Option<Filter>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    voice_gate: VoiceGate,
    mode: CaptureMode,
    ptt: PttController,
    echo: EchoSuppressor,
}

fn build_microphone_stream(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    capacity: usize,
    frames: &Arc<Mutex<VecDeque<f32>>>,
    muted: &Arc<AtomicBool>,
    level: &Arc<AtomicU32>,
) -> Result<cpal::Stream, String> {
    let channels = supported.channels() as usize;
    let sink = frames.clone();
    let cap_muted = muted.clone();
    let cap_level = level.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            supported.config(),
            move |input: &[f32], _| {
                push_captured_with(
                    input,
                    channels,
                    capacity,
                    &sink,
                    &cap_muted,
                    &cap_level,
                    |s| s,
                );
            },
            |error| tracing::error!("audio input failed: {error}"),
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            supported.config(),
            move |input: &[i16], _| {
                push_captured_with(
                    input, channels, capacity, &sink, &cap_muted, &cap_level, i16_to_f32,
                );
            },
            |error| tracing::error!("audio input failed: {error}"),
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            supported.config(),
            move |input: &[u16], _| {
                push_captured_with(
                    input, channels, capacity, &sink, &cap_muted, &cap_level, u16_to_f32,
                );
            },
            |error| tracing::error!("audio input failed: {error}"),
            None,
        ),
        other => {
            return Err(format!(
                "the input device uses {other} samples, which this client does not convert"
            ));
        }
    }
    .map_err(|error| format!("the input stream could not start: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("the input stream could not start: {error}"))?;
    Ok(stream)
}

impl Microphone {
    /// Opens the default input device.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("the machine has no default input device")?;
        let picked = pick_input(&device)?;
        let mut chosen = picked;
        let mut rate = chosen.sample_rate();
        let mut capacity = rate as usize * 2;
        let frames = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let muted = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let stream =
            match build_microphone_stream(&device, &chosen, capacity, &frames, &muted, &level) {
                Ok(stream) => stream,
                Err(first_error) => {
                    let fallback = device
                        .default_input_config()
                        .ok()
                        .and_then(|cfg| validate_default_config(cfg, "input").ok())
                        .filter(|cfg| {
                            cfg.sample_rate() != picked.sample_rate()
                                || cfg.channels() != picked.channels()
                                || cfg.sample_format() != picked.sample_format()
                        });
                    if let Some(default_cfg) = fallback {
                        tracing::warn!(
                            error = %first_error,
                            "preferred input stream failed; falling back to device default config"
                        );
                        chosen = default_cfg;
                        rate = chosen.sample_rate();
                        capacity = rate as usize * 2;
                        if let Ok(mut guard) = frames.lock() {
                            guard.reserve(capacity);
                        }
                        build_microphone_stream(
                            &device, &chosen, capacity, &frames, &muted, &level,
                        )?
                    } else {
                        return Err(first_error);
                    }
                }
            };
        Ok(Self {
            _stream: stream,
            frames,
            filter: (rate != WIRE_RATE)
                .then(|| Filter::new(rate, WIRE_RATE))
                .transpose()?,
            muted,
            level,
            voice_gate: VoiceGate::new()?,
            mode: CaptureMode::Vad,
            ptt: PttController::default(),
            echo: EchoSuppressor::default(),
        })
    }

    pub fn set_capture_mode(&mut self, mode: CaptureMode) {
        if self.mode != mode {
            self.mode = mode;
            self.ptt.cancel();
            let _ = self.voice_gate.reset();
            self.echo.reset();
        }
    }

    pub fn ptt_start(&mut self) {
        if self.mode == CaptureMode::Ptt {
            self.ptt.start();
        }
    }

    pub fn ptt_commit(&mut self) {
        if self.mode == CaptureMode::Ptt {
            self.ptt.commit();
        }
    }

    pub fn ptt_cancel(&mut self) {
        if self.mode == CaptureMode::Ptt {
            self.ptt.cancel();
            if let Ok(mut frames) = self.frames.try_lock() {
                frames.clear();
            }
            if let Some(filter) = &mut self.filter {
                filter.reset();
            }
        }
    }

    /// Mutes or unmutes microphone capture. Muting clears buffered frames and
    /// stops sending samples to the host.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.level.store(0f32.to_bits(), Ordering::Relaxed);
            if let Ok(mut frames) = self.frames.try_lock() {
                frames.clear();
            }
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// The latest captured microphone RMS amplitude, in 0..=1.
    pub fn level(&self) -> f32 {
        if self.is_muted() {
            0.
        } else {
            f32::from_bits(self.level.load(Ordering::Relaxed))
        }
    }

    /// Takes the frames captured since the last call as ordered wire chunks.
    pub fn take(&mut self, companion_speaking: bool, far_end_level: f32) -> CaptureOutput {
        if self.is_muted() {
            if let Ok(mut frames) = self.frames.lock() {
                frames.clear();
            }
            if let Some(filter) = &mut self.filter {
                filter.reset();
            }
            self.echo.reset();
            self.ptt.cancel();
            return CaptureOutput {
                chunks: self.voice_gate.reset().then(Vec::new).into_iter().collect(),
                open: false,
                barge_in: false,
            };
        }
        let samples: Vec<f32> = match self.frames.lock() {
            Ok(mut frames) => frames.drain(..).collect(),
            Err(_) => return CaptureOutput::default(),
        };
        if self.mode == CaptureMode::Ptt {
            let barge_in = self.ptt.active && companion_speaking;
            let converted = match self.filter.as_mut() {
                Some(filter) => filter.push(&samples).unwrap_or_default(),
                None => samples,
            };
            let mut out = self.ptt.process(&converted);
            out.barge_in = barge_in;
            return out;
        }
        let near_rms = self.level();
        let (allow_capture, barge_in) =
            self.echo
                .evaluate(companion_speaking, near_rms, far_end_level);
        if allow_capture {
            let samples = match self.filter.as_mut() {
                Some(filter) => filter.push(&samples),
                None => Ok(samples),
            };
            match samples.and_then(|samples| self.voice_gate.process(&samples)) {
                Ok(chunks) => {
                    return CaptureOutput {
                        chunks,
                        open: self.voice_gate.active,
                        barge_in,
                    };
                }
                Err(error) => tracing::warn!(%error, "microphone audio conversion failed"),
            }
        }
        // Keep pre-mute and pre-playback filter history out of the next utterance.
        if let Some(filter) = &mut self.filter {
            filter.reset();
        }
        CaptureOutput {
            chunks: self.voice_gate.reset().then(Vec::new).into_iter().collect(),
            open: false,
            barge_in: false,
        }
    }
}

fn push_captured_with<T: Copy>(
    input: &[T],
    channels: usize,
    capacity: usize,
    sink: &Mutex<VecDeque<f32>>,
    muted: &AtomicBool,
    level: &AtomicU32,
    to_f32: impl Fn(T) -> f32,
) {
    if muted.load(Ordering::Relaxed) || channels == 0 {
        level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    }
    let Ok(mut sink) = sink.try_lock() else {
        return;
    };
    const MAX_STACK_CH: usize = 16;
    let tracked = channels.min(MAX_STACK_CH);
    let mut ch_energy = [0f64; MAX_STACK_CH];
    let mut frames_count = 0usize;
    if channels > 1 {
        for frame in input.chunks_exact(channels) {
            for (idx, slot) in ch_energy.iter_mut().take(tracked).enumerate() {
                let s = f64::from(to_f32(frame[idx]));
                *slot += s * s;
            }
            frames_count += 1;
        }
    }
    let mut active_mask = [true; MAX_STACK_CH];
    let mut active_channels = channels;
    if channels > 1 && channels <= MAX_STACK_CH && frames_count > 0 {
        let max_energy = ch_energy[..tracked].iter().copied().fold(0.0f64, f64::max);
        if max_energy > 0.0 {
            let cutoff = max_energy * 0.05;
            let mut active = 0usize;
            for idx in 0..tracked {
                let keep = ch_energy[idx] >= cutoff;
                active_mask[idx] = keep;
                if keep {
                    active += 1;
                }
            }
            if active > 0 {
                active_channels = active;
            } else {
                active_mask.fill(true);
            }
        }
    }

    let mut energy = 0f64;
    let mut count = 0u64;
    for frame in input.chunks(channels) {
        let mono = if channels == 1 || frame.len() < channels || channels > MAX_STACK_CH {
            let sum: f32 = frame.iter().copied().map(&to_f32).sum();
            sum / frame.len().max(1) as f32
        } else {
            let mut sum = 0f32;
            for (idx, &sample) in frame.iter().enumerate() {
                if active_mask[idx] {
                    sum += to_f32(sample);
                }
            }
            sum / active_channels as f32
        };
        let mono = if mono.is_finite() { mono } else { 0. };
        energy += f64::from(mono) * f64::from(mono);
        count += 1;
        if sink.len() < capacity {
            sink.push_back(mono);
        }
    }
    let rms = if count == 0 {
        0.
    } else {
        (energy / count as f64).sqrt().clamp(0., 1.) as f32
    };
    level.store(rms.to_bits(), Ordering::Relaxed);
}

#[cfg(test)]
fn push_captured(
    input: &[f32],
    channels: usize,
    capacity: usize,
    sink: &Mutex<VecDeque<f32>>,
    muted: &AtomicBool,
    level: &AtomicU32,
) {
    push_captured_with(input, channels, capacity, sink, muted, level, |s| s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_gate_stays_closed_on_fluctuating_room_noise() {
        let noise: Vec<f32> = (0..VAD_FRAME_SAMPLES * 150)
            .map(|i| {
                let phase = i as f32;
                let amplitude = 0.008 + 0.020 * (phase * 0.004).sin().abs();
                0.02 + amplitude * (phase * 0.09).sin()
            })
            .collect();
        let white = (0..noise.len())
            .scan(11u32, |state, _| {
                *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                Some(0.06 * (*state as f32 / u32::MAX as f32 - 0.5))
            })
            .collect();
        for input in [
            noise.clone(),
            [vec![0.; WIRE_RATE as usize], noise].concat(),
            white,
        ] {
            let mut gate = VoiceGate::new().unwrap();
            // Callbacks do not necessarily end at a VAD frame boundary.
            for block in input.chunks(729) {
                assert!(gate.process(block).unwrap().is_empty());
            }
            assert!(!gate.active);
        }
    }

    #[test]
    fn voice_gate_keeps_preroll_and_separates_utterances_in_one_batch() {
        let mut gate = VoiceGate::new().unwrap();
        let mut audio = Vec::new();
        let mut chunks = Vec::new();
        let pre_roll = [0.01; VAD_FRAME_SAMPLES];
        let speech = [0.04; VAD_FRAME_SAMPLES];
        let silence = [0.0; VAD_FRAME_SAMPLES];
        gate.push_frame(&pre_roll, 1.0, &mut audio, &mut chunks);
        gate.push_frame(&silence, 0.0, &mut audio, &mut chunks);
        assert!(!gate.active, "one high score must not open the gate");

        for _ in 0..2 {
            for _ in 0..VAD_START_FRAMES {
                gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
            }
            assert!(gate.active);
            for _ in 0..=VAD_HANGOVER_FRAMES {
                gate.push_frame(&silence, 0.0, &mut audio, &mut chunks);
            }
            assert!(!gate.active);
        }

        let expected = [
            pre_roll.to_vec(),
            silence.to_vec(),
            speech.repeat(VAD_START_FRAMES as usize),
            silence.repeat(VAD_HANGOVER_FRAMES as usize),
        ]
        .concat();
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], encode(&expected));
        assert!(chunks[1].is_empty());
        assert_eq!(chunks[2], encode(&expected[VAD_FRAME_SAMPLES..]));
        assert!(chunks[3].is_empty());
    }

    #[test]
    fn voice_gate_keeps_the_frame_after_a_duration_split() {
        let mut gate = VoiceGate::new().unwrap();
        let mut audio = Vec::new();
        let mut chunks = Vec::new();
        let speech = [0.04; VAD_FRAME_SAMPLES];
        for _ in 0..VAD_START_FRAMES {
            gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
        }
        gate.active_frames = VAD_MAX_ACTIVE_FRAMES - 1;
        gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
        gate.push_frame(&speech, 0.8, &mut audio, &mut chunks);
        assert_eq!(
            chunks,
            [
                encode(&speech.repeat(VAD_START_FRAMES as usize + 1)),
                Vec::new()
            ]
        );
        assert_eq!(audio, speech);
    }

    #[test]
    fn stereo_capture_with_dead_channel_preserves_active_channel_amplitude() {
        let sink = Mutex::new(VecDeque::new());
        let muted = AtomicBool::new(false);
        let level = AtomicU32::new(0);
        let stereo_one_channel = [0.02f32, 0.0, -0.02, 0.0, 0.02, 0.0, -0.02, 0.0];
        push_captured(&stereo_one_channel, 2, 64, &sink, &muted, &level);
        let captured: Vec<f32> = sink.lock().unwrap().iter().copied().collect();
        assert_eq!(captured, [0.02, -0.02, 0.02, -0.02]);
        let rms = f32::from_bits(level.load(Ordering::Relaxed));
        assert!((rms - 0.02).abs() < 1e-6, "{rms}");
    }

    #[test]
    fn wire_pcm_round_trips_through_samples() {
        let samples = [-1., -0.5, 0., 0.5, 1.];
        let pcm = encode(&samples);
        assert_eq!(pcm.len(), samples.len() * 2);
        for (got, want) in decode(&pcm).unwrap().iter().zip(samples) {
            assert!((got - want).abs() < 1e-3, "{got} approximates {want}");
        }
    }

    #[test]
    fn an_odd_byte_count_is_rejected_rather_than_truncated() {
        assert!(decode(&[0, 1, 2]).is_err());
    }

    /// Duration has to survive conversion, which is the property a per-chunk
    /// resampler would lose at every boundary.
    #[test]
    fn a_streamed_conversion_preserves_the_sample_count_across_chunks() {
        let mut up = Filter::new(WIRE_RATE, 48_000).unwrap();
        let mut produced = 0usize;
        let mut fed = 0usize;
        // Uneven chunks on purpose: the wire delivers arbitrary frame counts.
        for chunk in [100usize, 500, 37, 900, 480, 211, 300] {
            let samples: Vec<f32> = (0..chunk)
                .map(|index| (index as f32 * 0.01).sin())
                .collect();
            fed += chunk;
            produced += up.push(&samples).unwrap().len();
        }
        let expected = (fed as f64 * 2.).round() as usize;
        // The filter holds at most one chunk of input plus its own delay.
        assert!(
            produced + CHUNK * 2 >= expected,
            "produced {produced} against about {expected}"
        );
        assert!(produced <= expected, "no more than the input produces");
    }

    /// The mouth follows what the device actually played: audible samples raise
    /// the level, and a drained deck reports silence.
    #[test]
    fn the_played_buffer_reports_its_rms_and_silence() {
        let deck = Arc::new(Mutex::new(Deck::default()));
        deck.lock()
            .unwrap()
            .pending
            .extend([0.5f32, -0.5, 0.5, -0.5]);
        let level = AtomicU32::new(0);
        let muted = AtomicBool::new(false);
        let mut output = vec![0.; 4];
        fill(&mut output, 1, &deck, &level, &muted);
        let rms = f32::from_bits(level.load(Ordering::Relaxed));
        assert!((rms - 0.5).abs() < 1e-6, "{rms}");

        let mut output = vec![0.; 4];
        fill(&mut output, 1, &deck, &level, &muted);
        assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
    }

    #[test]
    fn muted_playback_silences_output_and_level_while_advancing_deck() {
        let deck = Arc::new(Mutex::new(Deck::default()));
        {
            let mut guard = deck.lock().unwrap();
            guard.utterance = "u".into();
            guard.generation = 1;
            guard.pending.extend([0.5f32, -0.5, 0.5, -0.5]);
        }
        let level = AtomicU32::new(0);
        let muted = AtomicBool::new(true);
        let mut output = vec![1.; 4];
        fill(&mut output, 1, &deck, &level, &muted);
        assert_eq!(output, [0., 0., 0., 0.]);
        assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
        assert!(deck.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn muted_capture_drops_samples_and_clears_rms() {
        let sink = Mutex::new(VecDeque::new());
        let muted = AtomicBool::new(false);
        let level = AtomicU32::new(0);
        push_captured(&[0.5, -0.5, 0.5, -0.5], 2, 64, &sink, &muted, &level);
        assert_eq!(sink.lock().unwrap().len(), 2);
        assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);

        push_captured(&[0.5, 0.5, -0.5, -0.5], 2, 64, &sink, &muted, &level);
        let rms = f32::from_bits(level.load(Ordering::Relaxed));
        assert!((rms - 0.5).abs() < 1e-6, "{rms}");

        muted.store(true, Ordering::Relaxed);
        push_captured(&[0.9, 0.9], 2, 64, &sink, &muted, &level);
        assert_eq!(sink.lock().unwrap().len(), 4);
        assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
    }

    #[test]
    fn continuous_alsa_range_picks_wire_rate_and_mono_over_1hz_or_64ch() {
        let ranges = vec![
            cpal::SupportedStreamConfigRange::new(
                64,
                1,
                384_000,
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::F32,
            ),
            cpal::SupportedStreamConfigRange::new(
                1,
                1,
                384_000,
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::F32,
            ),
            cpal::SupportedStreamConfigRange::new(
                2,
                1,
                384_000,
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::I16,
            ),
        ];
        let picked = pick_config_from_ranges(ranges, "output")
            .unwrap()
            .expect("a config is picked");
        assert_eq!(picked.sample_rate(), WIRE_RATE);
        assert_eq!(picked.channels(), 1);
        assert_eq!(picked.sample_format(), cpal::SampleFormat::F32);
    }

    #[test]
    fn i16_only_hardware_range_is_accepted_and_converts_accurately() {
        let ranges = vec![
            cpal::SupportedStreamConfigRange::new(
                2,
                48_000,
                48_000,
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::U8,
            ),
            cpal::SupportedStreamConfigRange::new(
                2,
                48_000,
                48_000,
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::I16,
            ),
        ];
        let picked = pick_config_from_ranges(ranges, "input")
            .unwrap()
            .expect("i16 fallback is accepted");
        assert_eq!(picked.sample_rate(), 48_000);
        assert_eq!(picked.channels(), 2);
        assert_eq!(picked.sample_format(), cpal::SampleFormat::I16);

        let deck = Arc::new(Mutex::new(Deck::default()));
        {
            let mut guard = deck.lock().unwrap();
            guard.utterance = "u".into();
            guard.generation = 1;
            guard.pending.extend([0.5f32, -0.5]);
        }
        let level = AtomicU32::new(0);
        let muted = AtomicBool::new(false);
        let mut output_i16 = vec![0i16; 4];
        fill_with(&mut output_i16, 2, &deck, &level, &muted, 0i16, f32_to_i16);
        assert_eq!(output_i16[0], output_i16[1]);
        assert!(output_i16[0] > 16_000 && output_i16[2] < -16_000);
        assert!((f32::from_bits(level.load(Ordering::Relaxed)) - 0.5).abs() < 1e-4);

        let sink = Mutex::new(VecDeque::new());
        let cap_level = AtomicU32::new(0);
        let cap_muted = AtomicBool::new(false);
        push_captured_with(
            &output_i16,
            2,
            16,
            &sink,
            &cap_muted,
            &cap_level,
            i16_to_f32,
        );
        let captured: Vec<f32> = sink.lock().unwrap().iter().copied().collect();
        assert_eq!(captured.len(), 2);
        assert!((captured[0] - 0.5).abs() < 1e-3);
        assert!((captured[1] + 0.5).abs() < 1e-3);
        assert!((f32::from_bits(cap_level.load(Ordering::Relaxed)) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn b01_stop_current_and_reset_stream_do_not_silence_generation_one() {
        let mut queue = PlayerQueue::new(WIRE_RATE).unwrap();
        let pcm = encode(&[0.25; 64]);
        queue.push("u1", 5, &pcm).unwrap();
        assert!(!queue.deck.lock().unwrap().pending.is_empty());

        // Local stop clears buffered samples without latching flushed=u64::MAX.
        queue.stop_current();
        assert!(queue.deck.lock().unwrap().pending.is_empty());
        queue.push("u2", 6, &pcm).unwrap();
        assert!(!queue.deck.lock().unwrap().pending.is_empty());

        // After host restart or resync, reset_stream allows generation=1 again.
        queue.flush(6);
        queue.reset_stream();
        queue.push("u3", 1, &pcm).unwrap();
        assert_eq!(queue.deck.lock().unwrap().generation, 1);
        assert!(!queue.deck.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn q01_older_generation_chunk_never_overwrites_newer_generation() {
        let mut queue = PlayerQueue::new(WIRE_RATE).unwrap();
        let newer = encode(&[0.5; 32]);
        let older = encode(&[-0.5; 32]);
        queue.push("u3", 3, &newer).unwrap();
        queue.push("u2", 2, &older).unwrap();
        let deck = queue.deck.lock().unwrap();
        assert_eq!(deck.generation, 3);
        assert_eq!(deck.utterance, "u3");
        assert_eq!(deck.pending.len(), 32);
    }

    #[test]
    fn q02_utterance_switch_resets_filter_and_finish_drains_tail() {
        let mut queue = PlayerQueue::new(48_000).unwrap();
        // Less than one CHUNK (480 samples) stays buffered inside Filter.
        let partial = encode(&[0.25; 240]);
        queue.push("u1", 1, &partial).unwrap();
        assert!(queue.deck.lock().unwrap().pending.is_empty());

        // Switching to a new utterance resets Filter so u1's partial audio
        // never leaks into u2.
        let full = encode(&[-0.25; 480]);
        queue.push("u2", 2, &full).unwrap();
        {
            let deck = queue.deck.lock().unwrap();
            assert_eq!(deck.generation, 2);
            assert!(deck.pending.iter().all(|&s| s <= 0.05));
        }

        // Finishing an utterance drains the remaining resampler tail.
        let mut queue2 = PlayerQueue::new(48_000).unwrap();
        queue2.push("u3", 1, &partial).unwrap();
        assert!(queue2.deck.lock().unwrap().pending.is_empty());
        queue2.finish_utterance("u3", 1).unwrap();
        assert!(!queue2.deck.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn f07_ptt_mode_gates_capture_until_active_and_cancel_discards() {
        let mut ptt = PttController::default();
        // Inactive PTT ignores audio and emits nothing.
        let idle = ptt.process(&[0.2; 64]);
        assert!(!idle.open && idle.chunks.is_empty());

        // Start -> captures raw audio; commit -> emits end-of-utterance marker.
        ptt.start();
        let active = ptt.process(&[0.25; 64]);
        assert!(active.open);
        assert_eq!(active.chunks.len(), 1);
        assert!(!active.chunks[0].is_empty());
        ptt.commit();
        let committed = ptt.process(&[]);
        assert!(!committed.open);
        assert_eq!(committed.chunks, vec![Vec::<u8>::new()]);

        // Cancel -> discards buffered audio and emits NO end marker.
        ptt.start();
        let _ = ptt.process(&[0.3; 64]);
        ptt.cancel();
        let cancelled = ptt.process(&[0.3; 64]);
        assert!(!cancelled.open && cancelled.chunks.is_empty());
    }

    #[test]
    fn f08_echo_suppressor_blocks_echo_and_detects_barge_in() {
        let mut echo = EchoSuppressor::default();
        // Speaker echo closely tracking far-end level stays suppressed.
        for _ in 0..5 {
            assert_eq!(echo.evaluate(true, 0.12, 0.10), (false, false));
        }
        // Strong user voice exceeding far-end margin for 3 frames triggers barge-in once.
        assert_eq!(echo.evaluate(true, 0.45, 0.10), (false, false));
        assert_eq!(echo.evaluate(true, 0.45, 0.10), (false, false));
        assert_eq!(echo.evaluate(true, 0.45, 0.10), (true, true));
        // Subsequent frames stay open without re-triggering barge-in.
        assert_eq!(echo.evaluate(true, 0.45, 0.10), (true, false));
        // Once companion stops speaking, suppressor resets cleanly.
        assert_eq!(echo.evaluate(false, 0.01, 0.0), (true, false));
    }
}
