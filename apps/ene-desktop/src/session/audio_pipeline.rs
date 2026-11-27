//! Audio playback, microphone capture, mute/PTT state, and lip-sync telemetry.

use super::transport::SessionTransport;
use crate::{
    audio::{self, Microphone, Player},
    avatar::Avatar,
    state::Device,
    update::{Command, Update},
};
use ene_rpc as w;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;

pub(super) struct AudioPipeline {
    pub(super) player: Option<Player>,
    pub(super) microphone: Option<Microphone>,
    /// Software mute for the microphone.
    pub(super) mic_muted: bool,
    /// Active capture mode (VAD or Push-to-Talk) (`F07`).
    pub(super) capture_mode: audio::CaptureMode,
    pub(super) ptt_active: bool,
    /// Software mute for speaker output.
    pub(super) output_muted: bool,
    /// Last audio state forwarded to the UI, to avoid spamming idle frames.
    pub(super) last_ui_audio: (f32, f32, bool),
    /// Last amplitude sent per character, so silence sends nothing new.
    last_lip: HashMap<Arc<str>, f32>,
}

impl AudioPipeline {
    pub(super) fn new() -> Self {
        Self {
            player: None,
            microphone: None,
            mic_muted: false,
            capture_mode: audio::CaptureMode::Vad,
            ptt_active: false,
            output_muted: false,
            last_ui_audio: (0., 0., false),
            last_lip: HashMap::new(),
        }
    }

    pub(super) fn open_output(&mut self) -> Result<(), Device> {
        if self.player.is_some() {
            return Ok(());
        }
        match Player::open() {
            Ok(player) => {
                player.set_muted(self.output_muted);
                tracing::info!(
                    rate = player.rate(),
                    channels = player.channels(),
                    "the output device is open"
                );
                self.player = Some(player);
                Ok(())
            }
            Err(error) => {
                tracing::warn!("no audio output: {error}");
                Err(Device::Unavailable(error.to_string()))
            }
        }
    }

    pub(super) fn open_microphone(&mut self) -> Result<(), Device> {
        if self.microphone.is_some() {
            return Ok(());
        }
        match Microphone::open() {
            Ok(mut microphone) => {
                microphone.set_muted(self.mic_muted);
                microphone.set_capture_mode(self.capture_mode);
                self.microphone = Some(microphone);
                Ok(())
            }
            Err(error) => {
                tracing::warn!("no microphone: {error}");
                Err(Device::Unavailable(error.to_string()))
            }
        }
    }

    /// Plays one audio event. Only one utterance is audible at a time.
    pub(super) fn play(&mut self, event: &w::audio_output_event::Event) {
        match event {
            w::audio_output_event::Event::Chunk(chunk) => self.play_audio_chunk(chunk),
            w::audio_output_event::Event::Flush(flush) => self.play_audio_flush(flush),
            w::audio_output_event::Event::Finished(finished) => {
                self.play_audio_finished(finished);
            }
        }
    }

    fn play_audio_chunk(&mut self, chunk: &w::AudioChunkEnvelope) {
        tracing::debug!(
            utterance = %chunk.utterance_id,
            generation = chunk.generation_id,
            rate = chunk.sample_rate,
            companion = %chunk.companion_id,
            "audio output chunk"
        );
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if let Err(error) = player.push(
            &chunk.companion_id,
            &chunk.utterance_id,
            chunk.generation_id,
            &chunk.pcm_data,
        ) {
            tracing::warn!("the audio chunk was not played: {error}");
        }
    }

    fn play_audio_flush(&mut self, flush: &w::FlushControl) {
        if let Some(player) = self.player.as_mut() {
            player.flush(flush.flush_generation_id);
        }
        tracing::info!(reason = ?flush.reason, "the host flushed playback");
    }

    fn play_audio_finished(&mut self, finished: &w::UtteranceFinished) {
        if let Some(player) = self.player.as_mut()
            && let Err(error) =
                player.finish_utterance(&finished.utterance_id, finished.generation_id)
        {
            tracing::warn!("the audio resampler tail was not drained: {error}");
        }
        tracing::debug!(
            utterance = %finished.utterance_id,
            sentences = finished.total_sentences,
            "the host finished sending an utterance"
        );
    }

    /// Stops the current generation immediately.
    pub(super) fn stop_playback(&mut self) {
        if let Some(player) = self.player.as_mut() {
            player.stop_current();
        }
    }

    pub(super) fn clear_speaking_if(&mut self, companion_id: &str) {
        if self.playing_companion().as_deref() == Some(companion_id) {
            self.stop_playback();
        }
    }

    pub(super) fn playing_companion(&self) -> Option<Arc<str>> {
        self.player.as_ref()?.playback().companion_id
    }

    /// Sends the amplitude the device last played, so the renderer can open the
    /// mouth of the speaking companion in step with the audio.
    pub(super) fn pump_lip(
        &mut self,
        transport: &SessionTransport,
        avatar: Option<&mut Avatar>,
        updates: &UnboundedSender<Update>,
    ) {
        let playback = self.player.as_ref().map_or_default(Player::playback);
        let level = playback.level;
        let input = self.microphone.as_ref().map_or(0., Microphone::level);
        let speaking = playback.playing;
        let (prev_out, prev_in, prev_speaking) = self.last_ui_audio;
        if (level - prev_out).abs() >= 0.015
            || (input - prev_in).abs() >= 0.015
            || speaking != prev_speaking
        {
            self.last_ui_audio = (level, input, speaking);
            super::notify_ui(
                updates,
                Update::AudioLevel {
                    output: level,
                    input,
                    speaking,
                },
            );
        }
        let target = playback
            .companion_id
            .filter(|id| transport.bindings.contains(id.as_ref()));
        let Some(avatar) = avatar else {
            return;
        };
        self.last_lip.retain(|id, last| {
            target.as_ref() == Some(id)
                || (*last > 0.005 && !matches!(avatar.lip_sync(id, 0.), Ok(true)))
        });
        let Some(id) = target else {
            return;
        };
        if (self.last_lip.get(&id).copied().unwrap_or_default() - level).abs() < 0.005 {
            return;
        }
        if matches!(avatar.lip_sync(&id, level), Ok(true)) {
            tracing::trace!(companion = %id, "lip sync {level:.3}");
            self.last_lip.insert(id, level);
        }
    }

    pub(super) fn set_mic_muted(&mut self, muted: bool, updates: &UnboundedSender<Update>) {
        self.mic_muted = muted;
        if let Some(microphone) = self.microphone.as_mut() {
            microphone.set_muted(muted);
        }
        super::notify_ui(
            updates,
            Update::AudioMute {
                mic_muted: self.mic_muted,
                output_muted: self.output_muted,
            },
        );
    }

    pub(super) fn set_output_muted(&mut self, muted: bool, updates: &UnboundedSender<Update>) {
        self.output_muted = muted;
        if let Some(player) = self.player.as_ref() {
            player.set_muted(muted);
        }
        super::notify_ui(
            updates,
            Update::AudioMute {
                mic_muted: self.mic_muted,
                output_muted: self.output_muted,
            },
        );
    }

    pub(super) fn set_capture_mode(
        &mut self,
        mode: audio::CaptureMode,
        updates: &UnboundedSender<Update>,
    ) {
        self.capture_mode = mode;
        self.ptt_active = false;
        if let Some(microphone) = self.microphone.as_mut() {
            microphone.set_capture_mode(mode);
        }
        super::notify_ui(
            updates,
            Update::CaptureModeChanged {
                mode: self.capture_mode,
                ptt_active: self.ptt_active,
            },
        );
    }

    pub(super) fn handle_ptt(&mut self, command: &Command, updates: &UnboundedSender<Update>) {
        let changed = match command {
            Command::PttStart if self.capture_mode == audio::CaptureMode::Ptt => {
                self.ptt_active = true;
                if let Some(microphone) = self.microphone.as_mut() {
                    microphone.ptt_start();
                }
                true
            }
            Command::PttCommit if self.ptt_active => {
                self.ptt_active = false;
                if let Some(microphone) = self.microphone.as_mut() {
                    microphone.ptt_commit();
                }
                true
            }
            Command::PttCancel if self.ptt_active => {
                self.ptt_active = false;
                if let Some(microphone) = self.microphone.as_mut() {
                    microphone.ptt_cancel();
                }
                true
            }
            _ => false,
        };
        if changed {
            super::notify_ui(
                updates,
                Update::CaptureModeChanged {
                    mode: self.capture_mode,
                    ptt_active: self.ptt_active,
                },
            );
        }
    }

    pub(super) fn output_state(&self) -> Device {
        match self.player.as_ref() {
            Some(player) => Device::Ready(format!("{} Hz", player.rate())),
            None => Device::Off,
        }
    }

    pub(super) fn input_state(&self) -> Device {
        match self.microphone.as_ref() {
            Some(_) => Device::Ready("既定の入力デバイス".into()),
            None => Device::Off,
        }
    }
}
