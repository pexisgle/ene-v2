//! Playback and capture on the client's own devices.
//!
//! The host sends 24 kHz / 16-bit signed LE / mono PCM and expects the client
//! to convert it (`docs/design/voice-avatar.md` §4.3). This module owns that
//! conversion, the single-playback invariant, and the RMS the avatar uses for
//! lip sync.
//!
//! Two rules shape the code below. Only one utterance may be audible at a time,
//! so a new utterance or a flush stops the old one before anything else runs.
//! And a flush invalidates a generation, so a chunk that arrives late for a
//! flushed generation is dropped rather than played.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// The wire format the host and the client agree on.
pub const WIRE_RATE: u32 = 24_000;

/// What the mixer needs to know about the utterance being played.
#[derive(Default)]
struct Deck {
    /// The generation currently allowed to make sound.
    generation: u64,
    /// The utterance currently playing.
    utterance: String,
    /// Decoded, resampled, device-rate mono samples waiting to be played.
    pending: VecDeque<f32>,
    /// The RMS of the most recent block, for the avatar's lip sync.
    rms: f32,
}

/// The device-side player. Cloning is not allowed: one client owns one deck.
pub struct Player {
    deck: Arc<Mutex<Deck>>,
    rate: u32,
    channels: usize,
    /// The stream must stay alive for audio to continue; dropping it stops it.
    _stream: cpal::Stream,
    /// The generation the host last invalidated.
    flushed: Arc<AtomicU64>,
}

impl Player {
    /// Opens the default output device.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("the machine has no default output device")?;
        let supported = device
            .default_output_config()
            .map_err(|error| format!("the output device has no default config: {error}"))?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let deck = Arc::new(Mutex::new(Deck::default()));
        let flushed = Arc::new(AtomicU64::new(0));
        let sink = deck.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                supported.config(),
                move |output: &mut [f32], _| fill(output, channels, &sink),
                |error| tracing::error!("audio output failed: {error}"),
                None,
            ),
            other => {
                return Err(format!(
                    "the output device uses unsupported {other:?} samples"
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
            _stream: stream,
            flushed,
        })
    }

    /// Starts playing `utterance` at `generation`, replacing whatever played.
    ///
    /// A new utterance always wins: the old audio is dropped, never mixed.
    pub fn begin(&self, utterance: &str, generation: u64) {
        let mut deck = self.deck.lock().unwrap();
        deck.pending.clear();
        deck.utterance = utterance.to_owned();
        deck.generation = generation;
        deck.rms = 0.;
    }

    /// Queues one wire chunk. A chunk for a flushed or superseded generation is
    /// dropped, which is what makes a late arrival harmless.
    pub fn push(&self, utterance: &str, generation: u64, pcm: &[u8]) -> Result<(), String> {
        if generation <= self.flushed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let samples = decode(pcm)?;
        let resampled = resample(&samples, WIRE_RATE, self.rate)?;
        let mut deck = self.deck.lock().unwrap();
        if deck.utterance != utterance || deck.generation != generation {
            return Ok(());
        }
        deck.pending.extend(resampled);
        Ok(())
    }

    /// Stops everything at or below `generation`.
    pub fn flush(&self, generation: u64) {
        self.flushed.fetch_max(generation, Ordering::Relaxed);
        let mut deck = self.deck.lock().unwrap();
        if deck.generation <= generation {
            deck.pending.clear();
            deck.utterance.clear();
            deck.rms = 0.;
        }
    }

    /// Whether audio is still queued for the current utterance.
    pub fn playing(&self) -> bool {
        !self.deck.lock().unwrap().pending.is_empty()
    }

    /// The RMS of the last played block, for the avatar's lip sync.
    pub fn rms(&self) -> f32 {
        self.deck.lock().unwrap().rms
    }

    /// The device rate the wire audio is converted to.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The channel count the stream writes.
    pub fn channels(&self) -> usize {
        self.channels
    }
}

/// Fills one device buffer from the deck, duplicating mono into every channel.
fn fill(output: &mut [f32], channels: usize, deck: &Arc<Mutex<Deck>>) {
    let mut deck = deck.lock().unwrap();
    let mut energy = 0f32;
    let mut counted = 0usize;
    for frame in output.chunks_mut(channels) {
        let sample = deck.pending.pop_front().unwrap_or(0.);
        if sample != 0. || !deck.pending.is_empty() {
            energy += sample * sample;
            counted += 1;
        }
        for slot in frame.iter_mut() {
            *slot = sample;
        }
    }
    if counted > 0 {
        deck.rms = (energy / counted as f32).sqrt();
    } else {
        deck.rms = 0.;
    }
}

/// Decodes the wire's 16-bit signed LE mono PCM.
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

/// Encodes device samples back to the wire's 16-bit signed LE mono PCM.
pub fn encode(samples: &[f32]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let value = (sample.clamp(-1., 1.) * 32_767.).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    pcm
}

/// Resamples mono audio between two rates with the same filter the host uses.
pub fn resample(samples: &[f32], from: u32, to: u32) -> Result<Vec<f32>, String> {
    use rubato::{Fft, FixedSync, Resampler};
    if from == 0 || to == 0 {
        return Err("a sample rate of zero cannot be converted".into());
    }
    if from == to || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let expected = ((samples.len() as f64) * f64::from(to) / f64::from(from)).round() as usize;
    let mut resampler = Fft::<f32>::new(from as usize, to as usize, 1_024, 1, FixedSync::Input)
        .map_err(|error| format!("the sample rate cannot be converted: {error}"))?;
    let input =
        rubato::audioadapter_buffers::direct::InterleavedSlice::new(samples, 1, samples.len())
            .map_err(|error| format!("the sample rate cannot be converted: {error}"))?;
    let mut resampled = resampler
        .process_all(&input, samples.len(), None)
        .map_err(|error| format!("the sample rate cannot be converted: {error}"))?
        .take_data();
    resampled.truncate(expected);
    Ok(resampled)
}

/// The capture side: microphone frames at the wire rate.
///
/// Capture is opened only when the host has an STT connection; a client that
/// cannot open a microphone still plays audio.
pub struct Microphone {
    _stream: cpal::Stream,
    frames: Arc<Mutex<VecDeque<f32>>>,
}

impl Microphone {
    /// Opens the default input device and resamples it to the wire rate.
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("the machine has no default input device")?;
        let supported = device
            .default_input_config()
            .map_err(|error| format!("the input device has no default config: {error}"))?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let frames = Arc::new(Mutex::new(VecDeque::new()));
        let sink = frames.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                supported.config(),
                move |input: &[f32], _| {
                    // The first channel is enough: the wire is mono.
                    let mono: Vec<f32> = input
                        .chunks(channels)
                        .filter_map(<[f32]>::first)
                        .copied()
                        .collect();
                    if let Ok(resampled) = resample(&mono, rate, WIRE_RATE)
                        && let Ok(mut frames) = sink.lock()
                    {
                        frames.extend(resampled);
                        // A stalled reader must not grow without bound.
                        while frames.len() > WIRE_RATE as usize * 2 {
                            frames.pop_front();
                        }
                    }
                },
                |error| tracing::error!("audio input failed: {error}"),
                None,
            ),
            other => {
                return Err(format!(
                    "the input device uses unsupported {other:?} samples"
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
        })
    }

    /// Takes the frames captured since the last call, as wire PCM.
    pub fn take(&self) -> Vec<u8> {
        let mut frames = self.frames.lock().unwrap();
        let samples: Vec<f32> = frames.drain(..).collect();
        encode(&samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_pcm_round_trips_through_samples() {
        let samples = [-1., -0.5, 0., 0.5, 1.];
        let pcm = encode(&samples);
        assert_eq!(pcm.len(), samples.len() * 2);
        let decoded = decode(&pcm).unwrap();
        for (got, want) in decoded.iter().zip(samples) {
            assert!((got - want).abs() < 1e-3, "{got} approximates {want}");
        }
    }

    #[test]
    fn an_odd_byte_count_is_rejected_rather_than_truncated() {
        assert!(decode(&[0, 1, 2]).is_err());
    }

    #[test]
    fn resampling_preserves_duration() {
        let samples: Vec<f32> = (0..2_400).map(|i| (i as f32 * 0.01).sin()).collect();
        let up = resample(&samples, 24_000, 48_000).unwrap();
        assert!(
            up.len().abs_diff(4_800) <= 1,
            "24 kHz to 48 kHz doubles the length, got {}",
            up.len()
        );
        let down = resample(&up, 48_000, 24_000).unwrap();
        assert!(
            down.len().abs_diff(2_400) <= 1,
            "48 kHz to 24 kHz halves it, got {}",
            down.len()
        );
    }

    #[test]
    fn an_identical_rate_is_passed_through() {
        let samples = [0.25, -0.25];
        assert_eq!(resample(&samples, WIRE_RATE, WIRE_RATE).unwrap(), samples);
    }
}
