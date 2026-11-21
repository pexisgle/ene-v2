use super::AudioError;
use cpal::traits::DeviceTrait;

/// The wire format the host and the client agree on.
pub const WIRE_RATE: u32 = 24_000;

/// The chunk the resampler converts at a time. Small enough to keep latency
/// under a frame budget, large enough for the FFT filter to be efficient.
pub(super) const CHUNK: usize = 480;
pub(super) const U16_SILENCE: u16 = 32_768;

/// A streaming sample-rate converter that keeps its state across chunks.
pub(super) struct Filter {
    from: u32,
    to: u32,
    inner: rubato::Fft<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl Filter {
    pub(super) fn new(from: u32, to: u32) -> Result<Self, AudioError> {
        Self::with_chunk(from, to, CHUNK)
    }

    pub(super) fn with_chunk(from: u32, to: u32, chunk: usize) -> Result<Self, AudioError> {
        let inner = rubato::Fft::<f32>::new(
            from as usize,
            to as usize,
            chunk,
            1,
            rubato::FixedSync::Input,
        )
        .map_err(|error| AudioError::ResampleSetup(error.to_string()))?;
        Ok(Self {
            from,
            to,
            inner,
            pending: Vec::with_capacity(CHUNK * 2),
            output: vec![0.; CHUNK * 4],
        })
    }

    pub(super) fn reset(&mut self) {
        use rubato::Resampler;
        self.inner.reset();
        self.pending.clear();
    }

    pub(super) fn finish(&mut self) -> Result<Vec<f32>, AudioError> {
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
    pub(super) fn push(&mut self, input: &[f32]) -> Result<Vec<f32>, AudioError> {
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
            .map_err(|error| AudioError::Convert(error.to_string()))?;
            let frames = self.inner.output_frames_max();
            if self.output.len() < frames {
                self.output.resize(frames, 0.);
            }
            let mut sink = rubato::audioadapter_buffers::direct::InterleavedSlice::new_mut(
                &mut self.output[..frames],
                1,
                frames,
            )
            .map_err(|error| AudioError::Convert(error.to_string()))?;
            let (consumed, produced) = self
                .inner
                .process_into_buffer(&source, &mut sink, None)
                .map_err(|error| AudioError::Convert(error.to_string()))?;
            out.extend_from_slice(&self.output[..produced]);
            self.pending.drain(..consumed);
        }
        Ok(out)
    }
}

pub(super) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1., 1.) * 32_767.).round() as i16
}

pub(super) fn f32_to_u16(sample: f32) -> u16 {
    ((sample.clamp(-1., 1.) * 32_767.).round() + 32_768.) as u16
}

pub(super) fn i16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / 32_768.0
}

pub(super) fn u16_to_f32(sample: u16) -> f32 {
    (f32::from(sample) - 32_768.0) / 32_768.0
}

/// Decodes the wire's 16-bit signed little-endian mono PCM.
pub fn decode(pcm: &[u8]) -> Result<Vec<f32>, AudioError> {
    if !pcm.len().is_multiple_of(2) {
        return Err(AudioError::OddPcmBytes);
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
pub(super) fn pick_config_from_ranges(
    ranges: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
    direction: &'static str,
) -> Result<Option<cpal::SupportedStreamConfig>, AudioError> {
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
    if let Some((format, rate)) = rejected {
        return Err(AudioError::UnsupportedRangeFormat {
            direction,
            format,
            rate,
        });
    }
    Ok(None)
}

pub(super) fn validate_default_config(
    config: cpal::SupportedStreamConfig,
    direction: &'static str,
) -> Result<cpal::SupportedStreamConfig, AudioError> {
    if format_score(config.sample_format()) == 0 {
        return Err(AudioError::UnsupportedFormat {
            direction,
            format: config.sample_format(),
        });
    }
    if config.channels() == 0 {
        return Err(AudioError::ZeroChannels { direction });
    }
    Ok(config)
}

pub(super) fn pick_output(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, AudioError> {
    if let Ok(ranges) = device.supported_output_configs()
        && let Some(config) = pick_config_from_ranges(ranges, "output")?
    {
        return Ok(config);
    }
    let fallback = device
        .default_output_config()
        .map_err(|error| AudioError::NoDefaultConfig {
            direction: "output",
            reason: error.to_string(),
        })?;
    validate_default_config(fallback, "output")
}

pub(super) fn pick_input(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, AudioError> {
    if let Ok(ranges) = device.supported_input_configs()
        && let Some(config) = pick_config_from_ranges(ranges, "input")?
    {
        return Ok(config);
    }
    let fallback = device
        .default_input_config()
        .map_err(|error| AudioError::NoDefaultConfig {
            direction: "input",
            reason: error.to_string(),
        })?;
    validate_default_config(fallback, "input")
}
