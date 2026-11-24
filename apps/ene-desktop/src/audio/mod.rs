//! Playback and capture on the client's own devices.
//!
//! The wire format is fixed by the host: 24 kHz (or 16 kHz) 16-bit signed
//! little-endian mono PCM (`apps/ene-host/src/host/voice.rs`). The device format
//! is whatever the device reports, so this module queries it and either opens at
//! the wire rate directly or converts with `rubato`.

mod capture;
mod playback;
mod resampler;
#[cfg(test)]
mod tests;
mod vad;

pub use capture::Microphone;
pub use ene_client_core::CaptureMode;
pub use playback::Player;
pub use resampler::WIRE_RATE;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("the sample rate cannot be converted: {0}")]
    ResampleSetup(String),
    #[error("the audio was not converted: {0}")]
    Convert(String),
    #[error("the host sent an odd number of PCM bytes")]
    OddPcmBytes,
    #[error(
        "the {direction} device offers {format} at {rate} Hz, and this client converts only 32-bit float or 16-bit PCM"
    )]
    UnsupportedRangeFormat {
        direction: &'static str,
        format: cpal::SampleFormat,
        rate: u32,
    },
    #[error("the {direction} device uses {format} samples, which this client does not convert")]
    UnsupportedFormat {
        direction: &'static str,
        format: cpal::SampleFormat,
    },
    #[error("the {direction} device reports zero channels")]
    ZeroChannels { direction: &'static str },
    #[error("the {direction} device has no default config: {reason}")]
    NoDefaultConfig {
        direction: &'static str,
        reason: String,
    },
    #[error("the machine has no default {0} device")]
    NoDefaultDevice(&'static str),
    #[error("the {direction} stream could not start: {reason}")]
    StreamStart {
        direction: &'static str,
        reason: String,
    },
    #[error("the audio deck was poisoned")]
    DeckPoisoned,
}
