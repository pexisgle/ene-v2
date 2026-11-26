//! PCM wire-format helpers, WAV framing/downmix, and sample-rate conversion.

use crate::error::{HttpResult, VoiceHttpError};
use rubato::{Resampler, audioadapter_buffers::direct::InterleavedSlice};
use std::io::Cursor;
use wav_codec::{SampleFormat, WavReader, WavSpec, WavWriter};

const INVALID_WAV: &str = "the TTS endpoint did not return a valid WAV";
const MISSING_SAMPLE_RATE: &str = "the TTS engine returned audio without a sample rate";

pub const SAMPLE_RATE: u32 = 24_000;
pub const STT_SAMPLE_RATES: [u32; 2] = [24_000, 16_000];
pub type ResamplerCache = Option<StreamingResampler>;

pub fn wav_wrap(pcm: &[u8], sample_rate: u32) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err(VoiceHttpError::InvalidInput(
            "the audio has an odd number of PCM bytes".into(),
        ));
    }
    let data_len = u32::try_from(pcm.len())
        .map_err(|_| VoiceHttpError::LimitExceeded("the audio is too large for a WAV".into()))?;
    if sample_rate == 0
        || sample_rate.checked_mul(2).is_none()
        || data_len.checked_add(36).is_none()
    {
        return Err(VoiceHttpError::InvalidInput(
            "the audio sample rate or length cannot be written to a WAV".into(),
        ));
    }
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut wav = Vec::with_capacity(pcm.len().saturating_add(44));
    let mut writer =
        WavWriter::new(Cursor::new(&mut wav), spec).map_err(|error| wav_write_error(&error))?;
    for &sample in pcm.as_chunks::<2>().0 {
        writer
            .write_sample(i16::from_le_bytes(sample))
            .map_err(|error| wav_write_error(&error))?;
    }
    writer.finalize().map_err(|error| wav_write_error(&error))?;
    Ok(wav)
}

fn wav_write_error(error: &wav_codec::Error) -> VoiceHttpError {
    VoiceHttpError::InvalidInput(format!("the audio cannot be written to a WAV: {error}"))
}

pub fn parse_wav(bytes: &[u8]) -> HttpResult<(Vec<u8>, u32)> {
    let mut reader = WavReader::new(bytes).map_err(|error| wav_read_error(&error))?;
    let spec = reader.spec();
    if spec.sample_format != SampleFormat::Int
        || spec.bits_per_sample != 16
        || reader.spec_ex().bytes_per_sample != 2
    {
        return Err(VoiceHttpError::Decode(UNSUPPORTED_WAV.into()));
    }
    let channels = usize::from(spec.channels);
    // Bound the allocation by the actual response, not the declared chunk size.
    let mut pcm = Vec::with_capacity((reader.duration() as usize).min(bytes.len() / 2) * 2);
    let mut sum = 0i32;
    for (index, sample) in reader.samples::<i16>().enumerate() {
        sum += i32::from(sample.map_err(|error| wav_read_error(&error))?);
        if index % channels == channels - 1 {
            pcm.extend_from_slice(&((sum / i32::from(spec.channels)) as i16).to_le_bytes());
            sum = 0;
        }
    }
    Ok((pcm, spec.sample_rate))
}

const UNSUPPORTED_WAV: &str = "the TTS endpoint returned an unsupported WAV format";

fn wav_read_error(error: &wav_codec::Error) -> VoiceHttpError {
    let message = match error {
        wav_codec::Error::Unsupported(_) => UNSUPPORTED_WAV,
        wav_codec::Error::Format("no data chunk") => {
            "the TTS endpoint returned a WAV without audio data"
        }
        _ => INVALID_WAV,
    };
    VoiceHttpError::Decode(message.into())
}

pub fn wire_pcm(pcm: Vec<u8>, sample_rate: u32, cache: &mut ResamplerCache) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err(VoiceHttpError::Decode(
            "the TTS engine returned an odd number of PCM bytes".into(),
        ));
    }
    if sample_rate == 0 {
        return Err(VoiceHttpError::Protocol(MISSING_SAMPLE_RATE.into()));
    }
    if sample_rate == SAMPLE_RATE {
        return Ok(pcm);
    }
    resample_mono(&pcm, sample_rate, SAMPLE_RATE, cache)
}

pub fn resample_mono(
    pcm: &[u8],
    from: u32,
    to: u32,
    cache: &mut ResamplerCache,
) -> HttpResult<Vec<u8>> {
    if from == 0 {
        return Err(VoiceHttpError::Protocol(MISSING_SAMPLE_RATE.into()));
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
        Some(filter) if filter.from == from && filter.to == to => filter,
        slot => {
            slot.insert(StreamingResampler::with_chunk(from, to, 1_024).map_err(resample_error)?)
        }
    };
    let mut resampled = resampler.process_clip(&samples).map_err(resample_error)?;
    resampled.truncate(expected);
    let mut pcm = Vec::with_capacity(resampled.len() * 2);
    for sample in resampled {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    Ok(pcm)
}

fn resample_error(error: ResampleError) -> VoiceHttpError {
    let message = match error {
        ResampleError::Setup(message) | ResampleError::Convert(message) => message,
    };
    VoiceHttpError::Decode(format!(
        "the TTS sample rate cannot be converted: {message}"
    ))
}

/// The chunk the resampler converts at a time. Small enough to keep latency
/// under a frame budget, large enough for the FFT filter to be efficient.
const STREAM_CHUNK: usize = 480;

/// A streaming sample-rate converter that keeps its state across chunks.
pub struct StreamingResampler {
    from: u32,
    to: u32,
    inner: rubato::Fft<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
}

impl std::fmt::Debug for StreamingResampler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamingResampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("pending_frames", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl StreamingResampler {
    pub fn new(from: u32, to: u32) -> Result<Self, ResampleError> {
        Self::with_chunk(from, to, STREAM_CHUNK)
    }

    pub fn with_chunk(from: u32, to: u32, chunk: usize) -> Result<Self, ResampleError> {
        let inner = rubato::Fft::<f32>::new(
            from as usize,
            to as usize,
            chunk,
            1,
            rubato::FixedSync::Input,
        )
        .map_err(|error| ResampleError::Setup(error.to_string()))?;
        Ok(Self {
            from,
            to,
            inner,
            pending: Vec::with_capacity(chunk * 2),
            output: vec![0.; chunk * 4],
        })
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        self.pending.clear();
    }

    pub fn finish(&mut self) -> Result<Vec<f32>, ResampleError> {
        if self.pending.is_empty() {
            self.reset();
            return Ok(Vec::new());
        }
        let remainder = self.pending.len();
        let want = self.inner.input_frames_next();
        let pad_zeroes = want.saturating_sub(remainder % want);
        if pad_zeroes > 0 && !remainder.is_multiple_of(want) {
            self.pending.resize(remainder + pad_zeroes, 0.0);
        }
        let mut out = self.push(&[])?;
        let delay = self.inner.output_delay();
        let expected_tail =
            ((remainder as f64) * f64::from(self.to) / f64::from(self.from)).ceil() as usize;
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
    pub fn push(&mut self, input: &[f32]) -> Result<Vec<f32>, ResampleError> {
        self.pending.extend_from_slice(input);
        let mut out = Vec::new();
        loop {
            let want = self.inner.input_frames_next();
            if self.pending.len() < want {
                break;
            }
            let source =
                InterleavedSlice::new(&self.pending[..want], 1, want).map_err(convert_error)?;
            let frames = self.inner.output_frames_max();
            if self.output.len() < frames {
                self.output.resize(frames, 0.);
            }
            let mut sink = InterleavedSlice::new_mut(&mut self.output[..frames], 1, frames)
                .map_err(convert_error)?;
            let (consumed, produced) = self
                .inner
                .process_into_buffer(&source, &mut sink, None)
                .map_err(convert_error)?;
            out.extend_from_slice(&self.output[..produced]);
            self.pending.drain(..consumed);
        }
        Ok(out)
    }

    // The finite-clip path compensates for filter latency and resets the FFT
    // between utterances. Streaming playback keeps its own continuous state.
    fn process_clip(&mut self, samples: &[f32]) -> Result<Vec<f32>, ResampleError> {
        let input = InterleavedSlice::new(samples, 1, samples.len()).map_err(convert_error)?;
        let output = self
            .inner
            .process_all(&input, samples.len(), None)
            .map_err(convert_error)?;
        Ok(output.take_data())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("the sample rate cannot be converted: {0}")]
    Setup(String),
    #[error("the audio was not converted: {0}")]
    Convert(String),
}

fn convert_error(error: impl std::fmt::Display) -> ResampleError {
    ResampleError::Convert(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_conversion_preserves_the_sample_count_across_chunks() {
        let mut up = StreamingResampler::new(SAMPLE_RATE, 48_000).unwrap();
        let mut produced = 0usize;
        let mut fed = 0usize;
        for chunk in [100usize, 500, 37, 900, 480, 211, 300] {
            let samples: Vec<f32> = (0..chunk)
                .map(|index| (index as f32 * 0.01).sin())
                .collect();
            fed += chunk;
            produced += up.push(&samples).unwrap().len();
        }
        let expected = (fed as f64 * 2.).round() as usize;
        assert!(
            produced + STREAM_CHUNK * 2 >= expected,
            "produced {produced} against about {expected}"
        );
        assert!(produced <= expected, "no more than the input produces");
    }

    fn pcm_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
        samples.into_iter().flat_map(i16::to_le_bytes).collect()
    }

    #[test]
    fn finite_clips_keep_the_rounded_sample_count() {
        for from in [16_000, 24_000, 32_000, 44_100, 48_000, 96_000] {
            for len in [0, 1, 2, 7, 127, 1_023, 1_024, 1_025, 4_097, 24_001] {
                let pcm = pcm_bytes((0..len).map(|index| (index * 997) as i16));
                let converted = resample_mono(&pcm, from, SAMPLE_RATE, &mut None).unwrap();
                let expected = (len as f64 * f64::from(SAMPLE_RATE) / f64::from(from)).round();
                assert_eq!(converted.len(), expected as usize * 2, "{from}, {len}");
                if from == SAMPLE_RATE {
                    assert_eq!(converted, pcm);
                }
            }
        }
        assert_eq!(
            resample_mono(&[0, 0], 96_000, SAMPLE_RATE, &mut None)
                .unwrap()
                .len(),
            0
        );
        assert!(wire_pcm(vec![0], SAMPLE_RATE, &mut None).is_err());
        assert!(wire_pcm(Vec::new(), 0, &mut None).is_err());
    }

    #[test]
    fn reused_resamplers_match_cold_bytes_without_leaking_between_clips() {
        let mut cache = None;
        for from in [44_100, 48_000, 96_000, 16_000, 44_100] {
            for len in [0, 1, 2, 17, 127, 1_023, 1_024, 4_097, 24_001] {
                let impulse =
                    pcm_bytes((0..len).map(|index| if index == 0 { i16::MAX } else { 0 }));
                let signal = pcm_bytes((0..len).map(|index| (index * 997) as i16));
                let silence = vec![0; len * 2];
                for pcm in [&impulse, &silence, &signal, &silence, &signal] {
                    let cold = resample_mono(pcm, from, SAMPLE_RATE, &mut None).unwrap();
                    let reused = resample_mono(pcm, from, SAMPLE_RATE, &mut cache).unwrap();
                    assert_eq!(reused, cold, "{from}, {len}");
                    if pcm == &silence {
                        assert!(reused.iter().all(|byte| *byte == 0), "{from}, {len}");
                    }
                }
                let identity = wire_pcm(signal.clone(), SAMPLE_RATE, &mut cache).unwrap();
                assert_eq!(identity, signal);
                assert_eq!(wire_pcm(Vec::new(), from, &mut cache).unwrap().len(), 0);
            }
        }
        let pcm = pcm_bytes([i16::MIN, i16::MAX, 0, -123, 123]);
        for (from, to) in [(48_000, 16_000), (48_000, 24_000), (48_000, 16_000)] {
            assert_eq!(
                resample_mono(&pcm, from, to, &mut cache).unwrap(),
                resample_mono(&pcm, from, to, &mut None).unwrap(),
            );
        }
    }

    #[test]
    fn reads_pcm_after_an_odd_length_auxiliary_chunk() {
        let pcm = pcm_bytes([i16::MIN, 0, i16::MAX]);
        let mut wav = wav_wrap(&pcm, SAMPLE_RATE).unwrap();
        // JUNK has one payload byte followed by its RIFF alignment byte.
        wav.splice(36..36, *b"JUNK\x01\0\0\0x\0");
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        assert_eq!(parse_wav(&wav).unwrap(), (pcm, SAMPLE_RATE));
    }

    /// A WAV whose `data` chunk claims more bytes than the file holds is
    /// malformed. It must be refused, not silently truncated: truncation feeds
    /// a short buffer to the resampler and hides a corrupt response.
    #[test]
    fn a_data_chunk_larger_than_the_file_is_refused() {
        let pcm = [1i16.to_le_bytes(), 2i16.to_le_bytes()].concat();
        let good = wav_wrap(&pcm, 24_000).unwrap();
        assert_eq!(parse_wav(&good).unwrap(), (pcm.clone(), 24_000));

        let mut lying = good.clone();
        // Overwrite the `data` chunk size (bytes 40..44) with more than exists.
        lying[40..44].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(
            matches!(parse_wav(&lying), Err(VoiceHttpError::Decode(message)) if message == INVALID_WAV),
            "an oversized data chunk must be refused, got {:?}",
            parse_wav(&lying).map(|(pcm, _)| pcm.len())
        );
    }
}
