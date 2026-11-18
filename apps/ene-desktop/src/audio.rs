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
    inner: rubato::Fft<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Filter {
    fn new(from: u32, to: u32) -> Result<Self, String> {
        let inner = rubato::Fft::<f32>::new(
            from as usize,
            to as usize,
            CHUNK,
            1,
            rubato::FixedSync::Input,
        )
        .map_err(|error| format!("the sample rate cannot be converted: {error}"))?;
        Ok(Self {
            inner,
            pending: Vec::with_capacity(CHUNK * 2),
            output: vec![0.; CHUNK * 4],
        })
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

/// One sentence still queued for the device.
struct Sentence {
    sequence_no: u32,
    remaining: usize,
}

/// What the mixer knows about the utterance being played.
#[derive(Default)]
struct Deck {
    /// The generation currently allowed to make sound.
    generation: u64,
    utterance: String,
    /// Decoded, resampled, device-rate mono samples waiting to be played.
    pending: VecDeque<f32>,
    /// The sentences those samples belong to, in order.
    queue: VecDeque<Sentence>,
    /// Sentence indexes that played to the end. The host emits at most 64
    /// sentences per utterance, so one word holds every one of them.
    played: u64,
    /// Samples written to the device for the current utterance.
    played_samples: u64,
    /// Set when the device reported a stream error.
    failed: bool,
}

/// How far the audible stream has actually got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub utterance: String,
    pub generation: u64,
    /// Sentence indexes that played to their end, in order.
    pub played: Vec<u32>,
    pub played_samples: u64,
    /// Whether the queue is empty, which is what "played" means.
    pub drained: bool,
    pub failed: bool,
}

/// The device-side player. One client owns one deck.
pub struct Player {
    deck: Arc<Mutex<Deck>>,
    rate: u32,
    channels: usize,
    /// Device rate minus wire rate, when the device does not offer the wire
    /// rate. The session thread owns this, never the callback.
    filter: Option<Filter>,
    /// The stream must stay alive for audio to continue; dropping it stops it.
    _stream: cpal::Stream,
    /// The generation the host last invalidated.
    flushed: Arc<AtomicU64>,
    /// RMS of the buffer the device last played, for lip sync.
    level: Arc<AtomicU32>,
    /// Software mute: drains the deck and reports progress without audible output.
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
        let deck = Arc::new(Mutex::new(Deck::default()));
        let flushed = Arc::new(AtomicU64::new(0));
        let level = Arc::new(AtomicU32::new(0));
        let muted = Arc::new(AtomicBool::new(false));
        let sink = deck.clone();
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
            deck,
            rate,
            channels,
            filter: (rate != WIRE_RATE)
                .then(|| Filter::new(WIRE_RATE, rate))
                .transpose()?,
            _stream: stream,
            flushed,
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
    pub fn push(
        &mut self,
        utterance: &str,
        generation: u64,
        sequence_no: u32,
        pcm: &[u8],
    ) -> Result<(), String> {
        if generation <= self.flushed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let samples = decode(pcm)?;
        let samples = match self.filter.as_mut() {
            Some(filter) => filter.push(&samples)?,
            None => samples,
        };
        let mut deck = self
            .deck
            .lock()
            .map_err(|_| "the audio deck was poisoned")?;
        if deck.generation != generation || deck.utterance != utterance {
            // A new utterance always wins: the old audio is dropped, never mixed.
            deck.pending.clear();
            deck.queue.clear();
            deck.played = 0;
            deck.played_samples = 0;
            deck.failed = false;
            deck.generation = generation;
            deck.utterance = utterance.to_owned();
        }
        if let Some(last) = deck.queue.back_mut()
            && last.sequence_no == sequence_no
        {
            // Every chunk of one sentence shares its sequence number, so they
            // extend the same entry instead of looking like duplicates.
            last.remaining += samples.len();
        } else {
            deck.queue.push_back(Sentence {
                sequence_no,
                remaining: samples.len(),
            });
        }
        deck.pending.extend(samples);
        Ok(())
    }

    /// Stops everything at or below `generation`.
    pub fn flush(&self, generation: u64) {
        self.flushed.fetch_max(generation, Ordering::Relaxed);
        if let Ok(mut deck) = self.deck.lock()
            && deck.generation <= generation
        {
            deck.pending.clear();
            deck.queue.clear();
            deck.utterance.clear();
        }
    }

    /// Whether audio is still queued for the current utterance.
    pub fn playing(&self) -> bool {
        self.deck
            .lock()
            .map(|deck| !deck.pending.is_empty())
            .unwrap_or(false)
    }

    /// What actually reached the device, for `ReportPlaybackStatus`.
    pub fn progress(&self) -> Progress {
        let Ok(deck) = self.deck.lock() else {
            return Progress::default();
        };
        Progress {
            utterance: deck.utterance.clone(),
            generation: deck.generation,
            played: (0..64)
                .filter(|index| deck.played & (1 << index) != 0)
                .collect(),
            played_samples: deck.played_samples,
            drained: deck.pending.is_empty(),
            failed: deck.failed,
        }
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
        if sample != 0. || !deck.pending.is_empty() {
            deck.played_samples += 1;
        }
        energy += f64::from(sample) * f64::from(sample);
        if sample != 0. {
            audible += 1;
        }
        if let Some(sentence) = deck.queue.front_mut() {
            sentence.remaining = sentence.remaining.saturating_sub(1);
            if sentence.remaining == 0 {
                let finished = deck.queue.pop_front();
                if let Some(sentence) = finished
                    && sentence.sequence_no < 64
                {
                    deck.played |= 1 << sentence.sequence_no;
                }
            }
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
pub struct Microphone {
    _stream: cpal::Stream,
    frames: Arc<Mutex<VecDeque<f32>>>,
    filter: Option<Filter>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
}

impl Microphone {
    /// Opens the default input device.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("the machine has no default input device")?;
        let supported = pick_input(&device)?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        // Two seconds of headroom: past that, a stalled reader loses old audio
        // instead of growing without bound.
        let capacity = rate as usize * 2;
        let frames = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let muted = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
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
        Ok(Self {
            _stream: stream,
            frames,
            filter: (rate != WIRE_RATE)
                .then(|| Filter::new(rate, WIRE_RATE))
                .transpose()?,
            muted,
            level,
        })
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

    /// Takes the frames captured since the last call, as wire PCM.
    pub fn take(&mut self) -> Vec<u8> {
        if self.is_muted() {
            if let Ok(mut frames) = self.frames.lock() {
                frames.clear();
            }
            return Vec::new();
        }
        let samples: Vec<f32> = match self.frames.lock() {
            Ok(mut frames) => frames.drain(..).collect(),
            Err(_) => return Vec::new(),
        };
        let samples = match self.filter.as_mut() {
            Some(filter) => filter.push(&samples).unwrap_or_default(),
            None => samples,
        };
        encode(&samples)
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
    let mut energy = 0f64;
    let mut count = 0u64;
    for frame in input.chunks(channels) {
        // Average the channels: the wire is mono and the device
        // may route the microphone to any one of them.
        let sum: f32 = frame.iter().copied().map(&to_f32).sum();
        let mono = sum / channels as f32;
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

    #[test]
    fn a_sentence_keeps_one_queue_entry_across_its_chunks() {
        let deck = Arc::new(Mutex::new(Deck::default()));
        {
            let mut deck = deck.lock().unwrap();
            deck.utterance = "u".into();
            deck.generation = 1;
            for _ in 0..3 {
                match deck.queue.back_mut() {
                    Some(last) if last.sequence_no == 7 => last.remaining += 4,
                    _ => deck.queue.push_back(Sentence {
                        sequence_no: 7,
                        remaining: 4,
                    }),
                }
            }
            assert_eq!(deck.queue.len(), 1, "one sentence, one entry");
            assert_eq!(deck.queue[0].remaining, 12);
        }
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
            guard.queue.push_back(Sentence {
                sequence_no: 2,
                remaining: 4,
            });
            guard.pending.extend([0.5f32, -0.5, 0.5, -0.5]);
        }
        let level = AtomicU32::new(0);
        let muted = AtomicBool::new(true);
        let mut output = vec![1.; 4];
        fill(&mut output, 1, &deck, &level, &muted);
        assert_eq!(output, [0., 0., 0., 0.]);
        assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), 0.);
        let guard = deck.lock().unwrap();
        assert!(guard.pending.is_empty());
        assert_eq!(guard.played_samples, 4);
        assert_eq!(guard.played, 1 << 2);
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
            guard.queue.push_back(Sentence {
                sequence_no: 1,
                remaining: 2,
            });
            guard.pending.extend([0.5f32, -0.5]);
        }
        let level = AtomicU32::new(0);
        let muted = AtomicBool::new(false);
        let mut output_i16 = vec![0i16; 4];
        fill_with(&mut output_i16, 2, &deck, &level, &muted, 0i16, f32_to_i16);
        assert_eq!(output_i16[0], output_i16[1]);
        assert!(output_i16[0] > 16_000 && output_i16[2] < -16_000);
        assert!((f32::from_bits(level.load(Ordering::Relaxed)) - 0.5).abs() < 1e-4);
        assert_eq!(deck.lock().unwrap().played, 1 << 1);

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
}
