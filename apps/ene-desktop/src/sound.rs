use anyhow::{Context, Result};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ene_rpc::AudioOutput;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

type Signal = Arc<AtomicBool>;

pub(crate) struct Playback {
    stream: Option<cpal::Stream>,
    player: Arc<Mutex<Player>>,
    amplitude: Arc<AtomicU32>,
    pub companion: String,
    pub utterance: String,
}

struct Player {
    samples: VecDeque<f32>,
    phase: f64,
    rate: u32,
    ended: bool,
}

impl Playback {
    pub(crate) fn new() -> Self {
        Self {
            stream: None,
            player: Arc::new(Mutex::new(Player {
                samples: VecDeque::new(),
                phase: 0.0,
                rate: 24_000,
                ended: false,
            })),
            amplitude: Arc::new(AtomicU32::new(0)),
            companion: String::new(),
            utterance: String::new(),
        }
    }

    pub(crate) fn push(&mut self, packet: AudioOutput) -> Result<()> {
        if packet.stop {
            if packet.companion_id.is_empty() || packet.companion_id == self.companion {
                self.clear();
            }
            return Ok(());
        }
        if self.utterance != packet.utterance_id {
            self.clear();
            self.utterance = packet.utterance_id;
            self.companion = packet.companion_id;
        }
        if self.stream.is_none() {
            let device = cpal::default_host()
                .default_output_device()
                .context("no audio output device")?;
            let format = device.default_output_config()?;
            let config = format.config();
            let samples = self.player.clone();
            let amplitude = self.amplitude.clone();
            let stream = match format.sample_format() {
                SampleFormat::F32 => output::<f32>(&device, &config, samples, amplitude),
                SampleFormat::I16 => output::<i16>(&device, &config, samples, amplitude),
                SampleFormat::U16 => output::<u16>(&device, &config, samples, amplitude),
                _ => anyhow::bail!("unsupported output format"),
            }?;
            stream.play()?;
            self.stream = Some(stream);
        }
        if let Ok(mut player) = self.player.lock() {
            player.rate = packet.sample_rate;
            player.samples.extend(
                packet
                    .pcm
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|chunk| f32::from(i16::from_le_bytes(*chunk)) / 32768.0),
            );
            player.ended = packet.end;
            if player.samples.len() > (packet.sample_rate as usize * 300) {
                player.samples.clear();
            }
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        if let Ok(mut player) = self.player.lock() {
            player.samples.clear();
            player.phase = 0.0;
            player.ended = false;
        }
        self.utterance.clear();
        self.amplitude.store(0, Ordering::Relaxed);
    }

    pub(crate) fn rms(&self) -> f32 {
        f32::from_bits(self.amplitude.load(Ordering::Relaxed))
    }
}

fn output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    player: Arc<Mutex<Player>>,
    amplitude: Arc<AtomicU32>,
) -> Result<cpal::Stream, cpal::Error> {
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    device.build_output_stream(
        *config,
        move |buffer: &mut [T], _| {
            let mut energy = 0.0;
            let count = buffer.len() / channels;
            let mut player = player.lock().ok();
            for frame in buffer.chunks_mut(channels) {
                let mut sample = 0.0;
                if let Some(player) = player.as_mut()
                    && let Some(&a) = player.samples.front()
                {
                    let b = player.samples.get(1).copied().unwrap_or(a);
                    sample = a + (b - a) * player.phase as f32;
                    player.phase += f64::from(player.rate) / f64::from(rate);
                    let consumed = (player.phase as usize).min(player.samples.len());
                    player.samples.drain(..consumed);
                    if player.samples.is_empty() {
                        player.phase = 0.0;
                    } else {
                        player.phase -= consumed as f64;
                    }
                }
                energy += sample * sample;
                frame.fill(sample.to_sample::<T>());
            }
            amplitude.store(
                ((energy / count.max(1) as f32).sqrt() * 5.0)
                    .min(1.0)
                    .to_bits(),
                Ordering::Relaxed,
            );
        },
        |_| {},
        None,
    )
}

pub(crate) struct Capture {
    _stream: cpal::Stream,
    pub held: Signal,
    pub automatic: Signal,
    pub diagnostic: Signal,
    meter: Arc<AtomicU32>,
    segments: Arc<Mutex<VecDeque<Segment>>>,
}

pub(crate) enum Segment {
    Start {
        diagnostic: bool,
    },
    Audio {
        pcm: Vec<u8>,
        rate: u32,
        diagnostic: bool,
    },
}

struct Recorder {
    pcm: Vec<u8>,
    quiet: usize,
    diagnostic: bool,
}

impl Capture {
    pub(crate) fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("no microphone")?;
        let format = device.default_input_config()?;
        let config = format.config();
        let held = Arc::new(AtomicBool::new(false));
        let automatic = Arc::new(AtomicBool::new(false));
        let segments = Arc::new(Mutex::new(VecDeque::new()));
        let diagnostic = Arc::new(AtomicBool::new(false));
        let meter = Arc::new(AtomicU32::new(0));
        let make = CaptureSignals {
            held: held.clone(),
            automatic: automatic.clone(),
            segments: segments.clone(),
            diagnostic: diagnostic.clone(),
            meter: meter.clone(),
        };
        let stream = match format.sample_format() {
            SampleFormat::F32 => input::<f32>(&device, &config, make),
            SampleFormat::I16 => input::<i16>(&device, &config, make),
            SampleFormat::U16 => input::<u16>(&device, &config, make),
            _ => anyhow::bail!("unsupported microphone format"),
        }?;
        stream.play()?;
        Ok(Self {
            _stream: stream,
            held,
            automatic,
            diagnostic,
            meter,
            segments,
        })
    }

    pub(crate) fn level(&self) -> u32 {
        self.meter.load(Ordering::Relaxed)
    }

    pub(crate) fn take(&self) -> Vec<Segment> {
        self.segments
            .lock()
            .map(|mut segments| segments.drain(..).collect())
            .unwrap_or_default()
    }
}

struct CaptureSignals {
    held: Signal,
    automatic: Signal,
    diagnostic: Signal,
    meter: Arc<AtomicU32>,
    segments: Arc<Mutex<VecDeque<Segment>>>,
}

fn input<T: SizedSample>(
    device: &cpal::Device,
    config: &StreamConfig,
    signals: CaptureSignals,
) -> Result<cpal::Stream, cpal::Error>
where
    f32: FromSample<T>,
{
    let CaptureSignals {
        held,
        automatic,
        diagnostic,
        meter,
        segments,
    } = signals;
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let mut recorder = Recorder {
        pcm: Vec::new(),
        quiet: 0,
        diagnostic: false,
    };
    device.build_input_stream(
        *config,
        move |buffer: &[T], _| {
            let frames = buffer.chunks_exact(channels);
            let level = frames
                .clone()
                .take(1024)
                .map(|frame| frame[0].to_sample::<f32>().abs())
                .sum::<f32>()
                / frames.len().clamp(1, 1024) as f32;
            meter.store(
                (level.clamp(0., 1.) * 100.).round() as u32,
                Ordering::Relaxed,
            );
            let voice = level > 0.015;
            let auto = automatic.load(Ordering::Relaxed);
            let manual = held.load(Ordering::Relaxed);
            if !manual && !auto && recorder.pcm.is_empty() {
                return;
            }
            if voice {
                recorder.quiet = 0;
            } else {
                recorder.quiet += frames.len();
            }
            let recording = manual
                || (auto
                    && (voice || (!recorder.pcm.is_empty() && recorder.quiet < rate as usize / 2)));
            if recording && recorder.pcm.len() < 3 * 1024 * 1024 {
                if recorder.pcm.is_empty()
                    && let Ok(mut queue) = segments.lock()
                {
                    recorder.diagnostic = diagnostic.load(Ordering::Relaxed);
                    queue.push_back(Segment::Start {
                        diagnostic: recorder.diagnostic,
                    });
                }
                for frame in frames {
                    let sample = frame
                        .iter()
                        .map(|sample| (*sample).to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32;
                    recorder
                        .pcm
                        .extend(((sample * 32767.0).clamp(-32768.0, 32767.0) as i16).to_le_bytes());
                }
            } else if !recorder.pcm.is_empty() {
                let pcm = std::mem::take(&mut recorder.pcm);
                if pcm.len() >= rate as usize / 5
                    && let Ok(mut queue) = segments.lock()
                {
                    if queue.len() > 8 {
                        queue.pop_front();
                    }
                    queue.push_back(Segment::Audio {
                        pcm,
                        rate,
                        diagnostic: recorder.diagnostic,
                    });
                }
            }
        },
        |_| {},
        None,
    )
}
