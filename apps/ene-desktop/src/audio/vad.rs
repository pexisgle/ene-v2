use super::resampler::{Filter, WIRE_RATE, encode};
use std::collections::VecDeque;

const VAD_RATE: u32 = 16_000;
pub(super) const VAD_FRAME_SAMPLES: usize = WIRE_RATE as usize * 16 / 1_000;
pub(super) const VAD_START_FRAMES: u16 = 2; // 32 ms
pub(super) const VAD_HANGOVER_FRAMES: u16 = 38; // 608 ms
pub(super) const VAD_MAX_ACTIVE_FRAMES: u16 = 937; // about 15 s
const VAD_PRE_ROLL_SAMPLES: usize = VAD_FRAME_SAMPLES * 35; // 560 ms
const VAD_START_SCORE: f32 = 0.55;
const VAD_CONTINUE_SCORE: f32 = 0.35;

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

    pub(super) fn reset(&mut self) -> bool {
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
