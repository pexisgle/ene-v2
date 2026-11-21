//! Avatar renderer, audio playback/microphone, and OS perception bridge for [`Session`].

use super::{SAMPLE, SCREEN_SAMPLE, Session};
use crate::{
    audio::{Microphone, Player},
    avatar::{self, Avatar},
    host::Host,
    perception::{self, NotifyOn, Report, Sensor},
    state::{Device, Devices},
    update::Update,
};
use ene_avatar_ipc::{AvatarEvent, AvatarStatus};
use ene_rpc as w;
use std::{path::PathBuf, time::Duration};

impl Session {
    /// Sends the amplitude the device last played, so the renderer can open the
    /// mouth of the speaking companion in step with the audio.
    pub(super) fn pump_lip(&mut self) {
        let level = self.player.as_ref().map_or(0., Player::level);
        let input = self.microphone.as_ref().map_or(0., Microphone::level);
        let speaking = self.player.as_ref().is_some_and(Player::playing);
        let (prev_out, prev_in, prev_speaking) = self.last_ui_audio;
        if (level - prev_out).abs() >= 0.015
            || (input - prev_in).abs() >= 0.015
            || speaking != prev_speaking
        {
            self.last_ui_audio = (level, input, speaking);
            let _ = self.updates.send(Update::AudioLevel {
                output: level,
                input,
                speaking,
            });
        }
        let target = self
            .speaking_companion
            .as_deref()
            .filter(|id| self.bindings.contains_key(*id))
            .or_else(|| self.focused_companion_id())
            .map(str::to_owned);
        let Some(avatar) = self.avatar.as_mut() else {
            return;
        };
        let active_chars: Vec<String> = self.last_lip.keys().cloned().collect();
        for other in active_chars {
            if target.as_deref() != Some(other.as_str())
                && self.last_lip.get(&other).is_some_and(|v| *v > 0.005)
                && avatar.lip_sync(&other, 0.0).is_ok()
            {
                self.last_lip.insert(other, 0.0);
            }
        }
        let Some(id) = target else {
            return;
        };
        if self
            .last_lip
            .get(&id)
            .is_some_and(|last| (*last - level).abs() < 0.005)
        {
            return;
        }
        if avatar.lip_sync(&id, level).is_ok() {
            tracing::trace!(companion = %id, "lip sync {level:.3}");
            self.last_lip.insert(id, level);
        }
    }

    /// Stops playback now without poisoning future generations.
    pub(super) fn stop_playback(&mut self) {
        if let Some(player) = self.player.as_mut() {
            player.stop_current();
        }
    }

    /// Opens the avatar renderer, reporting why it is unavailable when it is.
    pub(super) async fn open_avatar(&mut self) {
        let Some(program) = self.options.renderer.clone() else {
            return;
        };
        match Avatar::spawn(&program) {
            Ok(avatar) => {
                self.avatar = Some(avatar);
                self.sync_avatars().await;
            }
            Err(error) => {
                tracing::warn!("the renderer did not start: {error}");
                self.set_avatar(Device::Unavailable(error.to_string()))
                    .await;
            }
        }
    }

    /// Reconciles all resident companions onto the running renderer with their
    /// resolved `BodyBinding` models and placements.
    pub(super) async fn sync_avatars(&mut self) {
        if self.avatar.is_none() {
            return;
        }
        let resident: Vec<(String, w::BodyBinding, Option<PathBuf>)> = self
            .bindings
            .keys()
            .map(|id| {
                let body = self.body_binding_for(id);
                let model = self.model_for(id);
                (id.clone(), body, model)
            })
            .collect();
        let idle = avatar::motion("idle");
        let default_scale = self.avatar_scale;
        if let Some(avatar) = self.avatar.as_mut() {
            remove_stale_avatars(avatar, &resident).await;
            for (id, body, model) in resident {
                let Some(model) = model else {
                    continue;
                };
                if let Err(error) = avatar
                    .show_with_body(&id, &model, idle.as_deref(), &body, default_scale)
                    .await
                {
                    tracing::warn!(character = %id, %error, "the renderer did not show the model");
                }
            }
        }
        self.devices().await;
    }

    /// Opens the output device once.
    pub(super) async fn open_devices(&mut self) {
        if self.player.is_none() {
            match Player::open() {
                Ok(player) => {
                    player.set_muted(self.output_muted);
                    tracing::info!(
                        rate = player.rate(),
                        channels = player.channels(),
                        "the output device is open"
                    );
                    self.player = Some(player);
                }
                Err(error) => {
                    tracing::warn!("no audio output: {error}");
                    self.set_out(Device::Unavailable(error.to_string())).await;
                    return;
                }
            }
        }
        self.devices().await;
    }

    pub(super) async fn open_microphone(&mut self) {
        if self.microphone.is_some() {
            self.devices().await;
            return;
        }
        match Microphone::open() {
            Ok(mut microphone) => {
                microphone.set_muted(self.mic_muted);
                microphone.set_capture_mode(self.capture_mode);
                self.microphone = Some(microphone);
                self.devices().await;
            }
            Err(error) => {
                tracing::warn!("no microphone: {error}");
                self.set_in(Device::Unavailable(error.to_string())).await;
            }
        }
    }

    /// Plays one audio event. Only one utterance is audible at a time.
    pub(super) async fn play(&mut self, event: w::audio_output_event::Event) {
        match event {
            w::audio_output_event::Event::Chunk(chunk) => self.play_audio_chunk(*chunk),
            w::audio_output_event::Event::Flush(flush) => self.play_audio_flush(*flush),
            w::audio_output_event::Event::Finished(finished) => self.play_audio_finished(*finished),
        }
    }

    fn play_audio_chunk(&mut self, chunk: w::AudioChunkEnvelope) {
        tracing::debug!(
            utterance = %chunk.utterance_id,
            generation = chunk.generation_id,
            rate = chunk.sample_rate,
            companion = %chunk.companion_id,
            "audio output chunk"
        );
        if !chunk.companion_id.is_empty() {
            self.speaking_companion = Some(chunk.companion_id.clone());
        }
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if let Err(error) = player.push(&chunk.utterance_id, chunk.generation_id, &chunk.pcm_data) {
            tracing::warn!("the audio chunk was not played: {error}");
        }
    }

    fn play_audio_flush(&mut self, flush: w::FlushControl) {
        if let Some(player) = self.player.as_mut() {
            player.flush(flush.flush_generation_id);
        }
        if !flush.companion_id.is_empty()
            && self.speaking_companion.as_deref() == Some(flush.companion_id.as_str())
        {
            self.speaking_companion = None;
        }
        tracing::info!(reason = ?flush.reason, "the host flushed playback");
    }

    fn play_audio_finished(&mut self, finished: w::UtteranceFinished) {
        if let Some(player) = self.player.as_mut()
            && let Err(error) =
                player.finish_utterance(&finished.utterance_id, finished.generation_id)
        {
            tracing::warn!("the audio resampler tail was not drained: {error}");
        }
        if !finished.companion_id.is_empty()
            && self.speaking_companion.as_deref() == Some(finished.companion_id.as_str())
        {
            self.speaking_companion = None;
        }
        tracing::debug!(
            utterance = %finished.utterance_id,
            sentences = finished.total_sentences,
            "the host finished sending an utterance"
        );
    }

    /// Returns to idle when the renderer finishes a gesture and detects dead renderers.
    pub(super) async fn drain_renderer(&mut self) {
        let Some(mut avatar) = self.avatar.take() else {
            return;
        };
        for event in avatar.notifications() {
            tracing::debug!(?event, "renderer");
            self.handle_renderer_event(&mut avatar, event).await;
        }
        if avatar.is_healthy() {
            self.avatar = Some(avatar);
        } else {
            avatar.shutdown();
            self.set_avatar(Device::Unavailable(
                "アバター描画プロセスが応答を停止しました".into(),
            ))
            .await;
        }
    }

    async fn handle_renderer_event(&mut self, avatar: &mut Avatar, event: AvatarEvent) {
        match event {
            AvatarEvent::Finished { character } => {
                if self.bindings.contains_key(&character)
                    && let Some(model) = self.model_for(&character)
                    && let Err(error) = avatar.return_to_idle(&character, &model).await
                {
                    tracing::warn!(%character, %error, "the renderer could not return to idle");
                }
            }
            AvatarEvent::State {
                character,
                status: AvatarStatus::Error,
                error,
            } => {
                tracing::warn!(%character, ?error, "the renderer reported a model error");
            }
            AvatarEvent::Click { character } => self.emit_avatar_click(character),
            AvatarEvent::State { .. } => {}
        }
    }

    fn emit_avatar_click(&mut self, character: String) {
        const DEBOUNCE: Duration = Duration::from_millis(500);
        let now = std::time::Instant::now();
        let allow = self
            .last_avatar_interaction
            .get(&character)
            .is_none_or(|prev| now.duration_since(*prev) >= DEBOUNCE);
        if !allow || !self.bindings.contains_key(&character) {
            return;
        }
        self.last_avatar_interaction.insert(character.clone(), now);
        let Some(host) = self.connection.read().ok().and_then(|guard| guard.clone()) else {
            return;
        };
        let event = w::PerceptionEvent {
            client_id: self.options.id.clone(),
            companion_id: Some(character.clone()),
            event: Some(w::perception_event::Event::AvatarInteraction(Box::new(
                w::AvatarInteraction {
                    kind: w::AvatarInteractionKind::Click.into(),
                    character_id: character,
                    region: "body".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        tokio::spawn(async move {
            let _ = host.client.report_perception(event).await;
        });
    }

    pub(super) fn set_eyes_enabled(&mut self, enabled: bool) {
        match perception::save_enabled(enabled) {
            Ok(()) => self.eyes_enabled = enabled,
            Err(error) => {
                if !enabled {
                    self.eyes_enabled = false;
                }
                let _ = self.updates.send(Update::Failed {
                    context: "「目」の設定を保存",
                    message: error.to_string(),
                });
            }
        }
        if !self.eyes_enabled {
            self.sensor = Sensor::default();
        }
        let _ = self.updates.send(Update::EyesEnabled(self.eyes_enabled));
    }

    #[cfg(windows)]
    pub(super) fn set_computer_use_enabled(&mut self, enabled: bool) {
        self.computer_frame = None;
        if !enabled {
            self.computer_enabled = false;
        }
        match perception::save_preference(crate::computer::PREFERENCE, enabled) {
            Ok(()) => self.computer_enabled = enabled,
            Err(error) => {
                let _ = self.updates.send(Update::Failed {
                    context: "画面操作の設定を保存",
                    message: error.to_string(),
                });
            }
        }
        let _ = self
            .updates
            .send(Update::ComputerUseEnabled(self.computer_enabled));
    }

    pub(super) fn perception_event(&self, event: w::perception_event::Event) -> w::PerceptionEvent {
        w::PerceptionEvent {
            client_id: self.options.id.clone(),
            subscription_id: self.subscription.clone(),
            event: Some(event),
            ..Default::default()
        }
    }

    /// Keeps socket waits and screenshot encoding off the audio/device loop.
    pub(super) fn sample(
        &mut self,
        host: &Host,
        screen_at: &mut tokio::time::Instant,
        eyes_open: &mut bool,
        observations: &mut tokio::task::JoinSet<()>,
    ) {
        use w::perception_event::Event;
        if !self.eyes_enabled
            || self.bindings.is_empty()
            || self.subscription.is_empty()
            || !perception::supported()
        {
            return;
        }
        #[cfg(windows)]
        if !perception::unlocked() {
            if std::mem::take(eyes_open) {
                let watch = self.sensor.watch.take();
                self.sensor = Sensor::default();
                self.sensor.apply(watch);
                let event = self.perception_event(Event::Enabled(false));
                let host = host.clone();
                observations.spawn(async move {
                    let _ =
                        tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
                });
            }
            return;
        }
        let now = tokio::time::Instant::now();
        let periodic = !*eyes_open || now >= *screen_at;
        let observed = perception::observe();
        let idle_seconds = observed.idle_seconds;
        let window = periodic.then(|| observed.window.clone()).flatten();
        let mut reports = self.sensor.observe(observed);
        let mut events = Vec::new();
        if periodic {
            *screen_at = now + SCREEN_SAMPLE;
            *eyes_open = true;
            events.push(Event::Enabled(true));
            if let Some(window) = window {
                reports.retain(|report| !matches!(report, Report::Window(_)));
                events.push(Event::ActiveWindow(Box::new(window_report(window))));
            }
            reports.retain(|report| !matches!(report, Report::Activity { .. }));
            reports.push(Report::Activity {
                is_active: idle_seconds < perception::IDLE_THRESHOLD,
                idle_seconds,
            });
        }
        events.extend(reports.into_iter().map(perception_report_event));
        if events.is_empty() {
            return;
        }
        let events = events
            .into_iter()
            .map(|event| self.perception_event(event))
            .collect::<Vec<_>>();
        let host = host.clone();
        #[cfg(windows)]
        let screen = periodic.then(|| self.perception_event(Event::ScreenFrame(Box::default())));
        observations.spawn(async move {
            for event in events {
                let result =
                    tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
                if !matches!(result, Ok(Ok(_))) {
                    tracing::debug!("perception was not reported");
                    return;
                }
            }
            #[cfg(windows)]
            if let Some(mut event) = screen
                && let Ok(Some(jpeg)) = tokio::task::spawn_blocking(perception::capture).await
            {
                event.event = Some(Event::ScreenFrame(Box::new(w::ScreenFrame {
                    jpeg,
                    ..Default::default()
                })));
                let _ = tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
            }
        });
    }

    /// Replaces the output device in the footer.
    pub(super) async fn set_out(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: self.avatar_state(),
            output: state,
            input: self.input_state(),
        })));
    }

    pub(super) async fn set_in(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: self.avatar_state(),
            output: self.output_state(),
            input: state,
        })));
    }

    pub(super) async fn set_avatar(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: state,
            output: self.output_state(),
            input: self.input_state(),
        })));
    }

    /// Sends the current device states.
    pub(super) async fn devices(&mut self) {
        let devices = Devices {
            avatar: self.avatar_state(),
            output: self.output_state(),
            input: self.input_state(),
        };
        let _ = self.updates.send(Update::Devices(Box::new(devices)));
    }

    fn avatar_state(&self) -> Device {
        match self.avatar.as_ref() {
            Some(avatar) => {
                let mut labels: Vec<String> = self
                    .bindings
                    .keys()
                    .filter_map(|id| avatar.describe(id))
                    .collect();
                if labels.is_empty() {
                    labels.push("描画中".into());
                }
                Device::Ready(labels.join(", "))
            }
            None if self.options.renderer.is_some() => Device::Off,
            None => Device::Unavailable("描画プロセスが設定されていません".into()),
        }
    }

    fn output_state(&self) -> Device {
        match self.player.as_ref() {
            Some(player) => Device::Ready(format!("{} Hz", player.rate())),
            None => Device::Off,
        }
    }

    fn input_state(&self) -> Device {
        match self.microphone.as_ref() {
            Some(_) => Device::Ready("既定の入力デバイス".into()),
            None => Device::Off,
        }
    }
}

async fn remove_stale_avatars(
    avatar: &mut Avatar,
    resident: &[(String, w::BodyBinding, Option<PathBuf>)],
) {
    let shown = avatar.shown_characters();
    for stale in shown {
        if !resident.iter().any(|(id, _, _)| id == &stale)
            && let Err(error) = avatar.remove_character(&stale).await
        {
            tracing::warn!(character = %stale, %error, "failed to remove released avatar");
        }
    }
}

fn perception_report_event(report: Report) -> w::perception_event::Event {
    use w::perception_event::Event;
    match report {
        Report::Window(window) => Event::ActiveWindow(Box::new(window_report(window))),
        Report::Interrupt(window, transition) => Event::WatchMatch(Box::new(w::WatchMatch {
            window: Some(window_report(window)).into(),
            transition: match transition {
                NotifyOn::Focus => w::WatchTransition::Focus,
                NotifyOn::Blur => w::WatchTransition::Blur,
                NotifyOn::TitleChange => w::WatchTransition::TitleChange,
            }
            .into(),
            ..Default::default()
        })),
        Report::Activity {
            is_active,
            idle_seconds,
        } => Event::UserActivity(Box::new(w::UserActivityState {
            is_active,
            idle_seconds,
            ..Default::default()
        })),
    }
}

fn window_report(window: perception::Window) -> w::ActiveWindowChanged {
    w::ActiveWindowChanged {
        title: window.title,
        app_id: window.app_id,
        ..Default::default()
    }
}
