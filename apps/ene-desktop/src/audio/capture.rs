use super::resampler::{
    Filter, WIRE_RATE, encode, i16_to_f32, pick_input, u16_to_f32, validate_default_config,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ene_client_core::CaptureMode;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

const VAD_RATE: u32 = 16_000;
pub(super) const VAD_FRAME_SAMPLES: usize = WIRE_RATE as usize * 16 / 1_000;
pub(super) const VAD_START_FRAMES: u16 = 2; // 32 ms
pub(super) const VAD_HANGOVER_FRAMES: u16 = 38; // 608 ms
pub(super) const VAD_MAX_ACTIVE_FRAMES: u16 = 937; // about 15 s
const VAD_PRE_ROLL_SAMPLES: usize = VAD_FRAME_SAMPLES * 35; // 560 ms
const VAD_START_SCORE: f32 = 0.55;
const VAD_CONTINUE_SCORE: f32 = 0.35;

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

fn is_single_tone(frame: &[f32]) -> bool {
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

pub(super) struct VoiceGate {
    detector: earshot::Detector,
    vad_filter: Filter,
    pub(super) active: bool,
    speech_frames: u16,
    hangover: u16,
    pub(super) active_frames: u16,
    pending: Vec<f32>,
    pre_roll: VecDeque<f32>,
}

impl VoiceGate {
    pub(super) fn new() -> Result<Self, super::AudioError> {
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

    pub(super) fn process(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, super::AudioError> {
        self.pending.extend_from_slice(samples);
        let mut chunks = Vec::new();
        let mut audio = Vec::new();
        let mut offset = 0;
        while let Some(frame) = self.pending[offset..]
            .first_chunk::<VAD_FRAME_SAMPLES>()
            .copied()
        {
            offset += VAD_FRAME_SAMPLES;
            let mut vad_frame = self.vad_filter.push(&frame)?;
            for sample in &mut vad_frame {
                *sample = if sample.is_finite() {
                    sample.clamp(-1., 1.)
                } else {
                    0.
                };
            }
            let score = self.detector.predict_f32(&vad_frame);
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

    pub(super) fn push_frame(
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
                    if let Ok(mut guard) = frames.lock() {
                        guard.reserve(capacity);
                    }
                    build_microphone_stream(&device, &chosen, capacity, &frames, &muted, &level)?
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

    pub fn level(&self) -> f32 {
        if self.is_muted() {
            0.
        } else {
            f32::from_bits(self.level.load(Ordering::Relaxed))
        }
    }

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
    let Ok(mut frames) = sink.frames.try_lock() else {
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
