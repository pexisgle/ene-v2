//! PCM wire-format helpers, WAV framing/downmix, and sample-rate conversion.

use crate::error::{HttpResult, VoiceHttpError};

pub const SAMPLE_RATE: u32 = 24_000;
pub const STT_SAMPLE_RATES: [u32; 2] = [24_000, 16_000];
pub type ResamplerCache = Option<(u32, u32, rubato::Fft<f32>)>;

pub fn wav_wrap(pcm: &[u8], sample_rate: u32) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err(VoiceHttpError::OddInputPcmBytes);
    }
    let data_len = u32::try_from(pcm.len()).map_err(|_| VoiceHttpError::WavTooLarge)?;
    if sample_rate == 0
        || sample_rate.checked_mul(2).is_none()
        || data_len.checked_add(36).is_none()
    {
        return Err(VoiceHttpError::WavInvalidParams);
    }
    let byte_rate = sample_rate * 2;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

pub fn parse_wav(bytes: &[u8]) -> HttpResult<(Vec<u8>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(VoiceHttpError::InvalidWav);
    }
    let mut offset = 12usize;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut audio_format = 0u16;
    let mut data_slice: Option<&[u8]> = None;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        offset += 8;
        let end = offset.saturating_add(size).min(bytes.len());
        let chunk = &bytes[offset..end];
        if id == b"fmt " && chunk.len() >= 16 {
            audio_format = u16::from_le_bytes([chunk[0], chunk[1]]);
            channels = u16::from_le_bytes([chunk[2], chunk[3]]);
            sample_rate = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            bits_per_sample = u16::from_le_bytes([chunk[14], chunk[15]]);
        } else if id == b"data" {
            data_slice = Some(chunk);
            break;
        }
        offset = end + (size % 2);
    }

    if audio_format != 1 || bits_per_sample != 16 || channels == 0 || sample_rate == 0 {
        return Err(VoiceHttpError::UnsupportedWavFormat);
    }
    let data = data_slice.ok_or(VoiceHttpError::MissingWavData)?;
    let ch = usize::from(channels);
    if ch == 1 {
        let even = data.len() - (data.len() % 2);
        return Ok((data[..even].to_vec(), sample_rate));
    }
    let frame_bytes = ch * 2;
    let mut pcm = Vec::with_capacity((data.len() / frame_bytes) * 2);
    for frame in data.chunks_exact(frame_bytes) {
        let mut sum = 0i32;
        for c in 0..ch {
            sum += i32::from(i16::from_le_bytes([frame[c * 2], frame[c * 2 + 1]]));
        }
        let mixed = (sum / ch as i32).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        pcm.extend_from_slice(&mixed.to_le_bytes());
    }
    Ok((pcm, sample_rate))
}

pub fn wire_pcm(pcm: Vec<u8>, sample_rate: u32, cache: &mut ResamplerCache) -> HttpResult<Vec<u8>> {
    if !pcm.len().is_multiple_of(2) {
        return Err(VoiceHttpError::OddOutputPcmBytes);
    }
    if sample_rate == 0 {
        return Err(VoiceHttpError::MissingSampleRate);
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
    use rubato::{Fft, FixedSync, Resampler};
    if from == 0 {
        return Err(VoiceHttpError::MissingSampleRate);
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
        Some((cached_from, cached_to, resampler)) if *cached_from == from && *cached_to == to => {
            resampler
        }
        slot => {
            let resampler = Fft::<f32>::new(from as usize, to as usize, 1_024, 1, FixedSync::Input)
                .map_err(|error| VoiceHttpError::Resample(error.to_string()))?;
            &mut slot.insert((from, to, resampler)).2
        }
    };
    let input =
        rubato::audioadapter_buffers::direct::InterleavedSlice::new(&samples, 1, samples.len())
            .map_err(|error| VoiceHttpError::Resample(error.to_string()))?;
    // `process_all` resets the cached filter between independent sentences.
    let mut resampled = resampler
        .process_all(&input, samples.len(), None)
        .map_err(|error| VoiceHttpError::Resample(error.to_string()))?
        .take_data();
    resampled.truncate(expected);
    let mut pcm = Vec::with_capacity(resampled.len() * 2);
    for sample in resampled {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            resample_mono(&[0, 0], 96_000, SAMPLE_RATE, &mut None)
                .unwrap()
                .is_empty()
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
                assert!(wire_pcm(Vec::new(), from, &mut cache).unwrap().is_empty());
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
}
