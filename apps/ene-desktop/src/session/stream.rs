//! Event-stream multiplexing, audio/capture pumping, and notification handling for [`Session`].

use super::{
    Binding, CAPTURE, LIP, Outcome, SAMPLE, Session, Step, commands::OperationError, persistent,
};
use crate::{
    audio::{self, Microphone, Player},
    host::Host,
    perception::Sensor,
    state::Device,
    update::{Command, Update},
    work_tools,
};
use ene_rpc as w;
use std::time::Duration;

type DeviceStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::DeviceEventView<'static>,
>;
type AudioOutStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::AudioOutputEventView<'static>,
>;
type MicSendHalf = connectrpc::client::BidiSendHalf<w::AudioInputChunk>;
type MicRecvHalf = connectrpc::client::BidiRecvHalf<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::TranscriptionEventView<'static>,
>;
type NotificationStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::NotificationView<'static>,
>;

struct PeriodicTimers {
    sample: tokio::time::Interval,
    capture: tokio::time::Interval,
    lip: tokio::time::Interval,
    observations: tokio::task::JoinSet<()>,
}

struct StreamLoopState {
    device: DeviceStream,
    subscription: NotificationStream,
    output: Option<AudioOutStream>,
    mic_send: Option<MicSendHalf>,
    mic_recv: Option<MicRecvHalf>,
    mic_retry_at: Option<tokio::time::Instant>,
    mic_error_reported: bool,
    screen_at: tokio::time::Instant,
    eyes_open: bool,
    timers: PeriodicTimers,
}

impl Session {
    /// Serves the device stream, audio, the microphone and the subscription.
    pub(super) async fn serve(&mut self, host: Host) -> Outcome {
        self.transport.subscription.clear();
        #[cfg(windows)]
        {
            self.devices.computer_frame = None;
        }
        let _ = self.updates.send(Update::EyesStatus(None));
        if self.transport.options.renderer.is_some() && self.devices.avatar.is_none() {
            self.open_avatar().await;
        }
        let device = match host
            .client
            .device_stream(w::Id {
                id: self.transport.options.id.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(stream) => stream,
            Err(error) => return self.transport.classify(error),
        };
        let subscription = match self.subscribe(&host).await {
            Ok(stream) => stream,
            Err(outcome) => return outcome,
        };
        let mut state = self.open_media_streams(&host, device, subscription).await;
        loop {
            if let Some(outcome) = self.serve_step(&host, &mut state).await {
                return outcome;
            }
        }
    }

    async fn serve_step(&mut self, host: &Host, state: &mut StreamLoopState) -> Option<Outcome> {
        tokio::select! {
            frame = state.device.message() => {
                let parsed = frame.map(|opt| opt.map(|m| m.to_owned_message()));
                self.handle_device_frame(host, parsed).await
            }
            Some(result) = self.devices.work.join_next(), if !self.devices.work.is_empty() => {
                self.finish_workspace_operation(host, result).await;
                None
            }
            frame = audio_message(&mut state.output) => {
                self.handle_audio_frame(state, frame).await
            }
            frame = transcription_message(&mut state.mic_recv) => {
                self.handle_transcription_frame(state, frame);
                None
            }
            () = retry_wait(state.mic_retry_at) => {
                self.handle_mic_retry(host, state).await;
                None
            }
            tick = next_periodic_tick(&mut state.timers) => {
                self.handle_periodic_tick(host, state, tick).await;
                None
            }
            frame = state.subscription.message() => {
                let parsed = frame.map(|opt| opt.map(|m| m.to_owned_message()));
                self.handle_subscription_frame(parsed).await
            }
            Some(command) = self.commands.recv() => {
                self.handle_loop_command(host, state, command).await
            }
            else => Some(Outcome::Closed),
        }
    }

    async fn handle_periodic_tick(
        &mut self,
        host: &Host,
        state: &mut StreamLoopState,
        tick: PeriodicTick,
    ) {
        match tick {
            PeriodicTick::Capture => self.handle_capture_tick(state).await,
            PeriodicTick::Lip => {
                if let Some(err_state) = self.devices.drain_renderer(&self.transport).await {
                    self.set_avatar(err_state).await;
                }
                self.audio
                    .pump_lip(&self.transport, self.devices.avatar.as_mut(), &self.updates);
            }
            PeriodicTick::Sample => {
                self.devices.sample(
                    &self.transport,
                    host,
                    &mut state.screen_at,
                    &mut state.eyes_open,
                    &mut state.timers.observations,
                );
            }
            PeriodicTick::ObservationDone => {}
        }
    }

    async fn finish_workspace_operation(
        &self,
        host: &Host,
        result: Result<w::DeviceOperationResult, tokio::task::JoinError>,
    ) {
        match result {
            Ok(result) => Self::report_operation(host, result).await,
            Err(error) => tracing::warn!("workspace operation did not finish: {error}"),
        }
    }

    async fn open_media_streams(
        &mut self,
        host: &Host,
        device: DeviceStream,
        subscription: NotificationStream,
    ) -> StreamLoopState {
        let output = self.open_audio_output_stream(host).await;
        let (mic_send, mic_recv) = self.open_audio_input_stream(host).await;
        StreamLoopState {
            device,
            subscription,
            output,
            mic_send,
            mic_recv,
            mic_retry_at: None,
            mic_error_reported: false,
            screen_at: tokio::time::Instant::now(),
            eyes_open: false,
            timers: PeriodicTimers {
                sample: interval(SAMPLE),
                capture: interval(CAPTURE),
                lip: interval(LIP),
                observations: tokio::task::JoinSet::new(),
            },
        }
    }

    async fn open_audio_output_stream(&mut self, host: &Host) -> Option<AudioOutStream> {
        if self.transport.bindings.is_empty() {
            return None;
        }
        match host
            .voice
            .subscribe_audio_output(w::SubscribeAudioRequest {
                client_id: self.transport.options.id.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(stream) => {
                if let Some(player) = self.audio.player.as_mut() {
                    player.reset_stream();
                }
                Some(stream)
            }
            Err(error) => {
                tracing::info!("no audio output stream: {error}");
                self.set_out(Device::Unavailable(error.to_string())).await;
                None
            }
        }
    }

    async fn open_audio_input_stream(
        &mut self,
        host: &Host,
    ) -> (Option<MicSendHalf>, Option<MicRecvHalf>) {
        if self.transport.bindings.is_empty() {
            return (None, None);
        }
        self.open_microphone().await;
        match host.voice.stream_audio_input().await {
            Ok(stream) => {
                let (send, recv) = stream.into_split();
                (Some(send), Some(recv))
            }
            Err(error) => {
                tracing::info!("no microphone stream: {error}");
                (None, None)
            }
        }
    }

    async fn handle_device_frame(
        &mut self,
        host: &Host,
        frame: Result<Option<w::DeviceEvent>, connectrpc::ConnectError>,
    ) -> Option<Outcome> {
        match frame {
            Ok(Some(event)) => {
                if let Some(event) = event.event {
                    match self.device_event(host, event).await {
                        Step::Continue => None,
                        Step::Restart => Some(Outcome::Resync("the binding changed".into())),
                        Step::Stop => Some(Outcome::Closed),
                    }
                } else {
                    None
                }
            }
            Ok(None) => Some(Outcome::Dropped("the host closed the device stream".into())),
            Err(error) => Some(self.transport.classify(error)),
        }
    }

    async fn handle_audio_frame(
        &mut self,
        state: &mut StreamLoopState,
        frame: Option<Result<w::audio_output_event::Event, connectrpc::ConnectError>>,
    ) -> Option<Outcome> {
        match frame {
            Some(Ok(event)) => {
                self.audio.play(event);
                None
            }
            Some(Err(error)) => {
                // A host without TTS answers `unimplemented` or
                // `failed_precondition`; that is a client without audio,
                // not a broken link.
                if persistent(&error) {
                    tracing::info!("the host has no audio output: {error}");
                    self.set_out(Device::Unavailable(error.to_string())).await;
                    state.output = None;
                    None
                } else {
                    Some(self.transport.classify(error))
                }
            }
            None => {
                state.output = None;
                None
            }
        }
    }

    fn handle_transcription_frame(
        &mut self,
        state: &mut StreamLoopState,
        frame: Option<Result<w::TranscriptionEvent, connectrpc::ConnectError>>,
    ) {
        match frame {
            Some(Ok(event)) => {
                state.mic_error_reported = false;
                let _ = self.updates.send(Update::Transcription {
                    text: event.text,
                    is_final: event.is_final,
                    is_speaking: event.is_speaking,
                });
            }
            Some(Err(error)) => {
                let _ = self.updates.send(Update::Transcription {
                    text: String::new(),
                    is_final: true,
                    is_speaking: false,
                });
                if persistent(&error) {
                    tracing::info!("the host has no speech recognition: {error}");
                    state.mic_retry_at = None;
                } else {
                    tracing::warn!("the transcription stream ended: {error}");
                    if !state.mic_error_reported {
                        state.mic_error_reported = true;
                        let _ = self.updates.send(Update::Failed {
                            context: "音声認識",
                            message: error.to_string(),
                        });
                    }
                    if !self.transport.bindings.is_empty() && self.audio.microphone.is_some() {
                        state.mic_retry_at =
                            Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    }
                }
                state.mic_recv = None;
                state.mic_send = None;
            }
            None => {
                let _ = self.updates.send(Update::Transcription {
                    text: String::new(),
                    is_final: true,
                    is_speaking: false,
                });
                if !self.transport.bindings.is_empty() && self.audio.microphone.is_some() {
                    state.mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                }
                state.mic_recv = None;
                state.mic_send = None;
            }
        }
    }

    async fn handle_mic_retry(&mut self, host: &Host, state: &mut StreamLoopState) {
        state.mic_retry_at = None;
        if self.transport.bindings.is_empty() || state.mic_recv.is_some() {
            return;
        }
        match host.voice.stream_audio_input().await {
            Ok(stream) => {
                let (send, recv) = stream.into_split();
                state.mic_send = Some(send);
                state.mic_recv = Some(recv);
                state.mic_error_reported = false;
            }
            Err(error) => {
                if !persistent(&error) {
                    state.mic_retry_at =
                        Some(tokio::time::Instant::now() + Duration::from_secs(10));
                }
            }
        }
    }

    async fn handle_capture_tick(&mut self, state: &mut StreamLoopState) {
        let speaking = self
            .audio
            .player
            .as_ref()
            .is_some_and(|player| player.playing() && !player.is_muted());
        let far_end_level = self.audio.player.as_ref().map_or(0., Player::level);
        let CaptureTick {
            chunks,
            state: capture_state,
            barge_in,
        } = capture_tick(
            &mut self.audio.microphone,
            !self.transport.bindings.is_empty(),
            &self.transport.options.id,
            speaking,
            far_end_level,
        );
        if barge_in && let Some(player) = self.audio.player.as_mut() {
            player.stop_current();
            self.audio.speaking_companion = None;
        }
        let mut sent = 0;
        if let Some(stream) = state.mic_send.as_mut() {
            for chunk in chunks {
                let bytes = chunk.pcm_data.len();
                if let Err(error) = stream.send(chunk).await {
                    tracing::info!("the microphone stream closed: {error}");
                    state.mic_send = None;
                    break;
                }
                sent += bytes;
            }
        }
        let _ = self.updates.send(Update::Capture {
            level: capture_state.level,
            open: capture_state.open,
            sent,
        });
    }

    async fn handle_subscription_frame(
        &mut self,
        frame: Result<Option<w::Notification>, connectrpc::ConnectError>,
    ) -> Option<Outcome> {
        match frame {
            Ok(Some(notification)) => match self.notification(notification).await {
                Step::Continue => None,
                Step::Restart => Some(Outcome::Resync("resync requested".into())),
                Step::Stop => Some(Outcome::Closed),
            },
            Ok(None) => Some(Outcome::Resync("the host closed the subscription".into())),
            Err(error) => Some(self.transport.classify(error)),
        }
    }

    async fn handle_loop_command(
        &mut self,
        host: &Host,
        state: &mut StreamLoopState,
        command: Command,
    ) -> Option<Outcome> {
        if matches!(command, Command::SetEyesEnabled(false)) {
            state.timers.observations.abort_all();
            if state.eyes_open {
                let event = self
                    .transport
                    .perception_event(w::perception_event::Event::Enabled(false));
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    host.client.report_perception(event),
                )
                .await;
            }
        }
        match self.command(host, command).await {
            Step::Continue => None,
            Step::Restart => Some(Outcome::Resync("the session settings changed".into())),
            Step::Stop => Some(Outcome::Closed),
        }
    }

    /// Opens the notification subscription, resuming from the last cursor.
    async fn subscribe(&mut self, host: &Host) -> Result<NotificationStream, Outcome> {
        match host
            .host
            .subscribe(w::SubscribeRequest {
                client_id: self.transport.options.id.clone(),
                cursor: self.transport.cursor.clone().into(),
                ..Default::default()
            })
            .await
        {
            Ok(stream) => Ok(stream),
            Err(error) => Err(self.transport.classify(error)),
        }
    }

    /// Handles one device event, executing only from the device stream.
    async fn device_event(&mut self, host: &Host, event: w::device_event::Event) -> Step {
        match event {
            w::device_event::Event::Ready(ready) => {
                self.transport.subscription = ready.subscription_id;
                Step::Continue
            }
            w::device_event::Event::Operation(operation) => {
                self.dispatch_device_operation(host, *operation).await;
                Step::Continue
            }
        }
    }

    async fn dispatch_device_operation(&mut self, host: &Host, operation: w::DeviceOperation) {
        let refusal = self.refusal(&operation);
        #[cfg(windows)]
        if operation.capability == w::COMPUTER_USE {
            let outcome = match refusal {
                Some(reason) => Err(reason.into()),
                None => Box::pin(self.computer_operation(host, &operation)).await,
            };
            let result = Self::operation_result(&self.transport.options.id, &operation, outcome);
            Self::report_operation(host, result).await;
            return;
        }
        if operation.capability.starts_with(work_tools::PREFIX) {
            let workspace = self.devices.workspace.clone();
            let active_work = self.devices.work.len();
            let request: Result<_, OperationError> = (|| {
                if let Some(reason) = refusal {
                    return Err(reason.into());
                }
                if operation.work_id.as_deref().is_none_or(str::is_empty) {
                    return Err(OperationError::WorkMissingSession);
                }
                if active_work >= 8 {
                    return Err(OperationError::WorkspaceBusy);
                }
                Ok((
                    workspace.ok_or(OperationError::NoWorkspace)?,
                    Self::operation_input(&operation)?,
                ))
            })();
            match request {
                Ok((workspace, input)) => {
                    let client_id = self.transport.options.id.clone();
                    self.devices.work.spawn(async move {
                        let output =
                            Box::pin(workspace.execute(&operation.capability, input)).await;
                        Self::operation_result(
                            &client_id,
                            &operation,
                            output.map_err(OperationError::from),
                        )
                    });
                }
                Err(reason) => {
                    let result =
                        Self::operation_result(&self.transport.options.id, &operation, Err(reason));
                    Self::report_operation(host, result).await;
                }
            }
        } else {
            let outcome = match refusal {
                Some(reason) => Err(reason.into()),
                None => Box::pin(self.execute(&operation)).await,
            };
            let result = Self::operation_result(&self.transport.options.id, &operation, outcome);
            Self::report_operation(host, result).await;
        }
    }

    /// Handles one host notification: forwards it, and tracks the cursor and the
    /// activation this client holds.
    async fn notification(&mut self, notification: w::Notification) -> Step {
        self.transport.cursor = notification.cursor.as_option().cloned();
        let mut restart = false;
        match notification.event.as_ref() {
            Some(w::notification::Event::ResyncRequired(_)) => {
                return Step::Restart;
            }
            Some(w::notification::Event::ActivationUpdated(activation)) => {
                restart = self.handle_activation_update(activation).await;
            }
            Some(w::notification::Event::Companion(companion)) => {
                self.transport
                    .companions
                    .insert(companion.id.clone(), (**companion).clone());
                if self.transport.bindings.contains_key(&companion.id) {
                    self.sync_avatars().await;
                }
                if self.transport.pending.as_deref() == Some(companion.id.as_str()) {
                    restart = true;
                }
            }
            Some(w::notification::Event::Generic(event)) if event.kind == "perception_status" => {
                if self.devices.eyes_enabled
                    && let Some(payload) = event.payload.as_option()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload.json)
                    && value["subscription_id"].as_str()
                        == Some(self.transport.subscription.as_str())
                {
                    let status = value["status"].as_str().map(str::to_owned);
                    let _ = self.updates.send(Update::EyesStatus(status));
                }
            }
            _ => {}
        }
        let _ = self
            .updates
            .send(Update::Notification(Box::new(notification.into())));
        if restart {
            Step::Restart
        } else {
            Step::Continue
        }
    }

    async fn handle_activation_update(&mut self, activation: &w::Activation) -> bool {
        let ours = activation.client_id.as_deref() == Some(self.transport.options.id.as_str());
        if ours {
            if !self
                .transport
                .bindings
                .contains_key(&activation.companion_id)
            {
                self.transport.bindings.insert(
                    activation.companion_id.clone(),
                    Binding {
                        companion_id: activation.companion_id.clone(),
                    },
                );
                if self.transport.focused.is_none() {
                    self.transport.focused = Some(activation.companion_id.clone());
                }
                self.sync_avatars().await;
                return self.transport.bindings.len() == 1;
            }
        } else if self
            .transport
            .bindings
            .remove(&activation.companion_id)
            .is_some()
        {
            self.audio.clear_speaking_if(&activation.companion_id);
            if self.transport.focused.as_deref() == Some(activation.companion_id.as_str()) {
                self.transport.focused = self.transport.bindings.keys().next().cloned();
            }
            self.sync_avatars().await;
            if self.transport.bindings.is_empty() {
                self.devices.sensor = Sensor::default();
                return true;
            }
        }
        false
    }
}

enum PeriodicTick {
    Capture,
    Lip,
    Sample,
    ObservationDone,
}

async fn next_periodic_tick(timers: &mut PeriodicTimers) -> PeriodicTick {
    tokio::select! {
        _ = timers.capture.tick() => PeriodicTick::Capture,
        _ = timers.lip.tick() => PeriodicTick::Lip,
        _ = timers.sample.tick(), if timers.observations.is_empty() => PeriodicTick::Sample,
        _ = timers.observations.join_next(), if !timers.observations.is_empty() => PeriodicTick::ObservationDone,
    }
}

/// A periodic timer that does not try to catch up after a stall.
fn interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// One frame from the audio stream. An absent stream parks this branch forever
/// instead of returning at once, which would spin a core.
async fn audio_message(
    output: &mut Option<AudioOutStream>,
) -> Option<Result<w::audio_output_event::Event, connectrpc::ConnectError>> {
    let Some(stream) = output.as_mut() else {
        return std::future::pending().await;
    };
    match stream.message().await {
        Ok(Some(message)) => Some(
            message
                .to_owned_message()
                .event
                .ok_or_else(|| connectrpc::ConnectError::internal("an empty audio event")),
        ),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// One live transcription event from the bidirectional voice input stream.
async fn transcription_message(
    input: &mut Option<MicRecvHalf>,
) -> Option<Result<w::TranscriptionEvent, connectrpc::ConnectError>> {
    let Some(stream) = input.as_mut() else {
        return std::future::pending().await;
    };
    match stream.message::<w::TranscriptionEvent>().await {
        Ok(Some(message)) => Some(Ok(message.to_owned_message())),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

async fn retry_wait(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// What one capture tick produced, and what the settings probe displays.
struct CaptureTick {
    chunks: Vec<w::AudioInputChunk>,
    state: CaptureState,
    barge_in: bool,
}

#[derive(Clone, Copy, Default)]
struct CaptureState {
    level: f32,
    open: bool,
}

/// Takes ordered PCM chunks and end markers from the microphone.
fn capture_tick(
    microphone: &mut Option<Microphone>,
    active: bool,
    client_id: &str,
    speaking: bool,
    far_end_level: f32,
) -> CaptureTick {
    let Some(microphone) = microphone.as_mut() else {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState::default(),
            barge_in: false,
        };
    };
    // The level is read before the gate so the probe shows the microphone even
    // while the gate is closed; that is the difference the test screen exists to
    // make visible.
    let level = microphone.level();
    if !active {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState { level, open: false },
            barge_in: false,
        };
    }
    let output = microphone.take(speaking, far_end_level);
    let state = CaptureState {
        level,
        open: output.open,
    };
    let barge_in = output.barge_in;
    let mut chunks: Vec<w::AudioInputChunk> = output
        .chunks
        .into_iter()
        .enumerate()
        .map(|(index, pcm_data)| w::AudioInputChunk {
            client_id: client_id.to_owned(),
            pcm_data,
            sample_rate: audio::WIRE_RATE,
            is_companion_speaking: speaking && !barge_in,
            barge_in: barge_in && index == 0,
            ..Default::default()
        })
        .collect();
    if barge_in && chunks.is_empty() {
        chunks.push(w::AudioInputChunk {
            client_id: client_id.to_owned(),
            sample_rate: audio::WIRE_RATE,
            barge_in: true,
            ..Default::default()
        });
    }
    CaptureTick {
        chunks,
        state,
        barge_in,
    }
}

/// What one operation claims about the client it was sent to.
pub(super) struct Claim<'a> {
    pub(super) client_id: &'a str,
    pub(super) subscription_id: &'a str,
    pub(super) work_id: Option<&'a str>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum AdmitError {
    #[error("this operation is addressed to `{0}`")]
    WrongClient(String),
    #[error("this operation belongs to an older device stream")]
    StaleStream,
    #[error("this client is not the active device for any companion")]
    NotActiveDevice,
}

/// Whether this client may execute the operation, or why it must not.
///
/// An operation executes when it names this client's own identity and its
/// current device stream, and either belongs to a work session bound to this
/// client or this client is the active device for a companion. The tests below
/// enumerate every combination of the fields, so this decision is checked in
/// full rather than sampled.
pub(super) fn admits(
    our_id: &str,
    our_subscription: &str,
    binding: Option<&Binding>,
    claim: &Claim<'_>,
) -> Result<(), AdmitError> {
    if claim.client_id != our_id {
        return Err(AdmitError::WrongClient(claim.client_id.to_owned()));
    }
    if !our_subscription.is_empty() && claim.subscription_id != our_subscription {
        return Err(AdmitError::StaleStream);
    }
    if claim.work_id.is_some_and(|work_id| !work_id.is_empty()) {
        return Ok(());
    }
    match binding {
        Some(_) => Ok(()),
        None => Err(AdmitError::NotActiveDevice),
    }
}
