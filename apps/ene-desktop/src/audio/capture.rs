use super::resampler::{
    WIRE_RATE, encode, i16_to_f32, pick_input, u16_to_f32, validate_default_config,
};
use super::vad::VoiceGate;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ene_client_core::CaptureMode;
use ene_voice::pcm::StreamingResampler;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

const ECHO_MARGIN: f32 = 1.8;
const MIN_BARGE_IN_RMS: f32 = 0.06;
const BARGE_IN_FRAMES: u8 = 3;

/// Far-end reference echo suppressor with barge-in detection (`F08`).
#[derive(Default)]
pub(super) struct EchoSuppressor {
    over_threshold_frames: u8,
    barge_in_latched: bool,
}

impl EchoSuppressor {
    fn reset(&mut self) {
        self.over_threshold_frames = 0;
        self.barge_in_latched = false;
    }

    pub(super) fn evaluate(
        &mut self,
        companion_speaking: bool,
        near_rms: f32,
        far_rms: f32,
    ) -> (bool, bool) {
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
pub(super) struct PttController {
    active: bool,
    has_audio: bool,
    pending_commit: bool,
}

impl PttController {
    pub(super) fn start(&mut self) {
        self.active = true;
        self.has_audio = false;
        self.pending_commit = false;
    }

    pub(super) fn commit(&mut self) {
        if self.active {
            self.active = false;
            self.pending_commit = self.has_audio;
            self.has_audio = false;
        }
    }

    pub(super) fn cancel(&mut self) {
        self.active = false;
        self.has_audio = false;
        self.pending_commit = false;
    }

    pub(super) fn process(&mut self, samples: &[f32]) -> CaptureOutput {
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
pub struct Microphone {
    _stream: cpal::Stream,
    frames: Arc<Mutex<VecDeque<f32>>>,
    filter: Option<StreamingResampler>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    voice_gate: VoiceGate,
    mode: CaptureMode,
    ptt: PttController,
    echo: EchoSuppressor,
}

pub(super) struct CaptureSink<'a> {
    pub(super) frames: &'a Mutex<VecDeque<f32>>,
    pub(super) muted: &'a AtomicBool,
    pub(super) level: &'a AtomicU32,
}

fn build_microphone_stream(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    capacity: usize,
    frames: &Arc<Mutex<VecDeque<f32>>>,
    muted: &Arc<AtomicBool>,
    level: &Arc<AtomicU32>,
) -> Result<cpal::Stream, super::AudioError> {
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
                    (channels, capacity),
                    CaptureSink {
                        frames: &sink,
                        muted: &cap_muted,
                        level: &cap_level,
                    },
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
                    input,
                    (channels, capacity),
                    CaptureSink {
                        frames: &sink,
                        muted: &cap_muted,
                        level: &cap_level,
                    },
                    i16_to_f32,
                );
            },
            |error| tracing::error!("audio input failed: {error}"),
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            supported.config(),
            move |input: &[u16], _| {
                push_captured_with(
                    input,
                    (channels, capacity),
                    CaptureSink {
                        frames: &sink,
                        muted: &cap_muted,
                        level: &cap_level,
                    },
                    u16_to_f32,
                );
            },
            |error| tracing::error!("audio input failed: {error}"),
            None,
        ),
        format => {
            return Err(super::AudioError::UnsupportedFormat {
                direction: "input",
                format,
            });
        }
    }
    .map_err(|error| super::AudioError::StreamStart {
        direction: "input",
        reason: error.to_string(),
    })?;
    stream
        .play()
        .map_err(|error| super::AudioError::StreamStart {
            direction: "input",
            reason: error.to_string(),
        })?;
    Ok(stream)
}

impl Microphone {
    /// Opens the default input device.
    pub fn open() -> Result<Self, super::AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(super::AudioError::NoDefaultDevice("input"))?;
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
                    let Some(default_cfg) = fallback else {
                        return Err(first_error);
                    };
                    tracing::warn!(
                        error = %first_error,
                        "preferred input stream failed; falling back to device default config"
                    );
                    chosen = default_cfg;
                    rate = chosen.sample_rate();
                    capacity = rate as usize * 2;
                    frames.lock().reserve(capacity);
                    build_microphone_stream(&device, &chosen, capacity, &frames, &muted, &level)?
                }
            };
        Ok(Self {
            _stream: stream,
            frames,
            filter: (rate != WIRE_RATE)
                .then(|| StreamingResampler::new(rate, WIRE_RATE))
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
            let _discarded = self.voice_gate.reset();
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
            if let Some(mut frames) = self.frames.try_lock() {
                frames.clear();
            }
            if let Some(filter) = &mut self.filter {
                filter.reset();
            }
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.level.store(0f32.to_bits(), Ordering::Relaxed);
            if let Some(mut frames) = self.frames.try_lock() {
                frames.clear();
            }
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn level(&self) -> f32 {
        if self.is_muted() {
            0.
        } else {
            f32::from_bits(self.level.load(Ordering::Relaxed))
        }
    }

    pub fn take(&mut self, companion_speaking: bool, far_end_level: f32) -> CaptureOutput {
        if self.is_muted() {
            self.frames.lock().clear();
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
        let samples: Vec<f32> = self.frames.lock().drain(..).collect();
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
                Some(filter) => filter.push(&samples).map_err(super::AudioError::from),
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

const MAX_STACK_CH: usize = 16;

fn select_active_channels<T: Copy>(
    input: &[T],
    channels: usize,
    to_f32: &impl Fn(T) -> f32,
) -> ([bool; MAX_STACK_CH], usize) {
    let mut active_mask = [true; MAX_STACK_CH];
    let mut active_channels = channels;
    if channels <= 1 || channels > MAX_STACK_CH {
        return (active_mask, active_channels);
    }
    let mut ch_energy = [0f64; MAX_STACK_CH];
    let mut frames_count = 0usize;
    for frame in input.chunks_exact(channels) {
        for (idx, slot) in ch_energy.iter_mut().take(channels).enumerate() {
            let s = f64::from(to_f32(frame[idx]));
            *slot += s * s;
        }
        frames_count += 1;
    }
    if frames_count == 0 {
        return (active_mask, active_channels);
    }
    let max_energy = ch_energy[..channels].iter().copied().fold(0.0f64, f64::max);
    if max_energy <= 0.0 {
        return (active_mask, active_channels);
    }
    let cutoff = max_energy * 0.05;
    let mut active = 0usize;
    for idx in 0..channels {
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
    (active_mask, active_channels)
}

pub(super) fn push_captured_with<T: Copy>(
    input: &[T],
    (channels, capacity): (usize, usize),
    sink: CaptureSink<'_>,
    to_f32: impl Fn(T) -> f32,
) {
    if sink.muted.load(Ordering::Relaxed) || channels == 0 {
        sink.level.store(0f32.to_bits(), Ordering::Relaxed);
        return;
    }
    let Some(mut frames) = sink.frames.try_lock() else {
        return;
    };
    let (active_mask, active_channels) = select_active_channels(input, channels, &to_f32);
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
        if frames.len() < capacity {
            frames.push_back(mono);
        }
    }
    let rms = if count == 0 {
        0.
    } else {
        (energy / count as f64).sqrt().clamp(0., 1.) as f32
    };
    sink.level.store(rms.to_bits(), Ordering::Relaxed);
}
