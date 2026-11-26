use super::{
    AudioError,
    resampler::{U16_SILENCE, WIRE_RATE, decode, f32_to_i16, f32_to_u16, pick_output},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ene_voice::pcm::StreamingResampler;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

/// What the mixer knows about the utterance being played.
#[derive(Default)]
pub(super) struct Deck {
    pub(super) generation: u64,
    pub(super) utterance: String,
    pub(super) pending: VecDeque<f32>,
}

pub(super) struct PlayerQueue {
    pub(super) deck: Arc<Mutex<Deck>>,
    filter: Option<StreamingResampler>,
    flushed: Arc<AtomicU64>,
}

impl PlayerQueue {
    pub(super) fn new(device_rate: u32) -> Result<Self, AudioError> {
        Ok(Self {
            deck: Arc::new(Mutex::new(Deck::default())),
            filter: (device_rate != WIRE_RATE)
                .then(|| StreamingResampler::new(WIRE_RATE, device_rate))
                .transpose()?,
            flushed: Arc::new(AtomicU64::new(0)),
        })
    }

    pub(super) fn push(
        &mut self,
        utterance: &str,
        generation: u64,
        pcm: &[u8],
    ) -> Result<(), AudioError> {
        if generation == 0 || generation <= self.flushed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let decoded = decode(pcm)?;
        let switched = {
            let deck = self.deck.lock();
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
        let mut deck = self.deck.lock();
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

    pub(super) fn finish_utterance(
        &mut self,
        utterance: &str,
        generation: u64,
    ) -> Result<(), AudioError> {
        if generation == 0 || generation <= self.flushed.load(Ordering::Relaxed) {
            if let Some(filter) = self.filter.as_mut() {
                filter.reset();
            }
            return Ok(());
        }
        let active = {
            let deck = self.deck.lock();
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
        let mut deck = self.deck.lock();
        if generation > self.flushed.load(Ordering::Relaxed)
            && deck.generation == generation
            && deck.utterance == utterance
        {
            deck.pending.extend(tail);
        }
        Ok(())
    }

    pub(super) fn stop_current(&mut self) {
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        let mut deck = self.deck.lock();
        deck.pending.clear();
        deck.utterance.clear();
    }

    pub(super) fn flush(&mut self, generation: u64) {
        self.flushed.fetch_max(generation, Ordering::Relaxed);
        let mut deck = self.deck.lock();
        if deck.generation <= generation {
            deck.pending.clear();
            deck.utterance.clear();
            if let Some(filter) = self.filter.as_mut() {
                filter.reset();
            }
        }
    }

    pub(super) fn reset_stream(&mut self) {
        self.flushed.store(0, Ordering::Relaxed);
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
        let mut deck = self.deck.lock();
        deck.generation = 0;
        deck.pending.clear();
        deck.utterance.clear();
    }
}

/// The device-side player. One client owns one deck.
pub struct Player {
    queue: PlayerQueue,
    rate: u32,
    channels: usize,
    _stream: cpal::Stream,
    level: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
}

impl Player {
    /// Opens the default output device, preferring a configuration that needs no
    /// conversion.
    pub fn open() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoDefaultDevice("output"))?;
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
                    fill_with(
                        output,
                        channels,
                        PlaybackSink {
                            deck: &sink,
                            level: &rms,
                            muted: &out_muted,
                        },
                        0.0f32,
                        |s| s,
                    );
                },
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                supported.config(),
                move |output: &mut [i16], _| {
                    fill_with(
                        output,
                        channels,
                        PlaybackSink {
                            deck: &sink,
                            level: &rms,
                            muted: &out_muted,
                        },
                        0i16,
                        f32_to_i16,
                    );
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
                        PlaybackSink {
                            deck: &sink,
                            level: &rms,
                            muted: &out_muted,
                        },
                        U16_SILENCE,
                        f32_to_u16,
                    );
                },
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            format => {
                return Err(AudioError::UnsupportedFormat {
                    direction: "output",
                    format,
                });
            }
        }
        .map_err(|error| AudioError::StreamStart {
            direction: "output",
            reason: error.to_string(),
        })?;
        stream.play().map_err(|error| AudioError::StreamStart {
            direction: "output",
            reason: error.to_string(),
        })?;
        Ok(Self {
            queue,
            rate,
            channels,
            _stream: stream,
            level,
            muted,
        })
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.level.store(0f32.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn push(&mut self, utterance: &str, generation: u64, pcm: &[u8]) -> Result<(), AudioError> {
        self.queue.push(utterance, generation, pcm)
    }

    pub fn finish_utterance(&mut self, utterance: &str, generation: u64) -> Result<(), AudioError> {
        self.queue.finish_utterance(utterance, generation)
    }

    pub fn stop_current(&mut self) {
        self.queue.stop_current();
    }

    pub fn flush(&mut self, generation: u64) {
        self.queue.flush(generation);
    }

    pub fn reset_stream(&mut self) {
        self.queue.reset_stream();
    }

    pub fn playing(&self) -> bool {
        !self.queue.deck.lock().pending.is_empty()
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

pub(super) struct PlaybackSink<'a> {
    pub(super) deck: &'a Mutex<Deck>,
    pub(super) level: &'a AtomicU32,
    pub(super) muted: &'a AtomicBool,
}

/// Fills one device buffer from the deck, duplicating mono into every channel.
pub(super) fn fill_with<T: Copy>(
    output: &mut [T],
    channels: usize,
    sink: PlaybackSink<'_>,
    silence: T,
    to_device: impl Fn(f32) -> T,
) {
    if channels == 0 {
        output.fill(silence);
        sink.level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    }
    let Some(mut deck) = sink.deck.try_lock() else {
        // The session thread is queueing; silence one buffer rather than block.
        output.fill(silence);
        sink.level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    };
    let is_muted = sink.muted.load(Ordering::Relaxed);
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
    // The RMS of what this buffer actually sent is the mouth opening.
    let rms = if is_muted || audible == 0 {
        0.
    } else {
        (energy / audible as f64).sqrt() as f32
    };
    sink.level.store(rms.to_bits(), Ordering::Relaxed);
}
