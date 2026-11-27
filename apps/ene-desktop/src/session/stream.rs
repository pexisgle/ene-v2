//! Event-stream multiplexing and notification handling for [`Session`].
//!
//! The audio and capture branches of the loop live in [`super::stream_audio`].

#[cfg(windows)]
use super::device_commands::OperationOutput;
use super::{
    CAPTURE, LIP, Outcome, SAMPLE, Session, Step,
    commands::OperationError,
    stream_audio::{
        AudioOutStream, MicRecvHalf, MicSendHalf, audio_message, handle_audio_frame,
        handle_capture_tick, handle_mic_retry, handle_transcription_frame, open_audio_input_stream,
        open_audio_output_stream, retry_wait, transcription_message,
    },
    workspace_calls::WorkspaceCalls,
};
use crate::{
    host::Host,
    perception::Sensor,
    update::{Command, Update},
    work_tools,
};
use ene_rpc as w;
use std::time::Duration;

type DeviceStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    w::__buffa::view::DeviceEventView<'static>,
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

pub(super) struct StreamLoopState {
    device: DeviceStream,
    work: WorkspaceCalls,
    subscription: NotificationStream,
    pub(super) output: Option<AudioOutStream>,
    pub(super) mic_send: Option<MicSendHalf>,
    pub(super) mic_recv: Option<MicRecvHalf>,
    pub(super) mic_retry_at: Option<tokio::time::Instant>,
    pub(super) mic_error_reported: bool,
    screen_at: tokio::time::Instant,
    eyes_open: bool,
    timers: PeriodicTimers,
}

impl Session {
    /// Serves the device stream, audio, the microphone and the subscription.
    pub(super) async fn serve(&mut self, host: Host) -> Outcome {
        self.transport.subscription.clear();
        self.devices.sensor.reset();
        #[cfg(windows)]
        {
            self.devices.computer_frame = None;
        }
        super::notify_ui(&self.updates, Update::EyesStatus(None));
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
            Err(error) => return self.transport.classify(&error),
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
                self.handle_device_frame(host, &mut state.work, parsed).await
            }
            Some(result) = state.work.finished() => {
                WorkspaceCalls::report(host, result).await;
                None
            }
            frame = audio_message(&mut state.output) => {
                handle_audio_frame(self, state, frame).await
            }
            frame = transcription_message(&mut state.mic_recv) => {
                handle_transcription_frame(self, state, frame);
                None
            }
            () = retry_wait(state.mic_retry_at) => {
                handle_mic_retry(self, host, state).await;
                None
            }
            tick = next_periodic_tick(&mut state.timers) => {
                self.handle_periodic_tick(host, state, tick).await;
                None
            }
            frame = state.subscription.message() => {
                let parsed = frame.map(|opt| opt.map(|m| m.to_owned_message()));
                self.handle_subscription_frame(&state.work, parsed).await
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
            PeriodicTick::Capture => handle_capture_tick(self, state).await,
            PeriodicTick::Lip => {
                if self.devices.drain_renderer(&self.transport).await {
                    self.report_devices().await;
                }
                self.audio
                    .pump_lip(&self.transport, self.devices.avatar.as_mut(), &self.updates);
            }
            PeriodicTick::Sample => {
                if self.transport.published_body != self.devices.body_available(&self.transport) {
                    self.transport.sync_capabilities(host, &self.devices).await;
                }
                // Fire-and-forget reports are only reaped here; without this
                // the set grew for the life of the session.
                self.devices.reap_background();
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

    async fn open_media_streams(
        &mut self,
        host: &Host,
        device: DeviceStream,
        subscription: NotificationStream,
    ) -> StreamLoopState {
        let output = open_audio_output_stream(self, host).await;
        let (mic_send, mic_recv) = open_audio_input_stream(self, host).await;
        StreamLoopState {
            device,
            work: WorkspaceCalls::new(),
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

    async fn handle_device_frame(
        &mut self,
        host: &Host,
        work: &mut WorkspaceCalls,
        frame: Result<Option<w::DeviceEvent>, connectrpc::ConnectError>,
    ) -> Option<Outcome> {
        match frame {
            Ok(Some(event)) => {
                if let Some(event) = event.event {
                    match self.device_event(host, work, event).await {
                        Step::Continue => None,
                        Step::Restart => Some(Outcome::Resync("the binding changed".into())),
                        Step::Stop => Some(Outcome::Closed),
                    }
                } else {
                    None
                }
            }
            Ok(None) => Some(Outcome::Dropped("the host closed the device stream".into())),
            Err(error) => Some(self.transport.classify(&error)),
        }
    }

    async fn handle_subscription_frame(
        &mut self,
        work: &WorkspaceCalls,
        frame: Result<Option<w::Notification>, connectrpc::ConnectError>,
    ) -> Option<Outcome> {
        match frame {
            Ok(Some(notification)) => {
                work.on_notification(&notification);
                match self.notification(notification).await {
                    Step::Continue => None,
                    Step::Restart => Some(Outcome::Resync("resync requested".into())),
                    Step::Stop => Some(Outcome::Closed),
                }
            }
            Ok(None) => Some(Outcome::Resync("the host closed the subscription".into())),
            Err(error) => Some(self.transport.classify(&error)),
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
                let _waited = tokio::time::timeout(
                    Duration::from_secs(1),
                    host.client.report_perception(event),
                )
                .await;
            }
        }
        match super::window_commands::command(self, host, command).await {
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
            Err(error) => Err(self.transport.classify(&error)),
        }
    }

    /// Handles one device event, executing only from the device stream.
    async fn device_event(
        &mut self,
        host: &Host,
        work: &mut WorkspaceCalls,
        event: w::device_event::Event,
    ) -> Step {
        match event {
            w::device_event::Event::Ready(ready) => {
                self.transport.subscription = ready.subscription_id;
                Step::Continue
            }
            w::device_event::Event::Operation(operation) => {
                self.dispatch_device_operation(host, work, *operation).await;
                Step::Continue
            }
        }
    }

    async fn dispatch_device_operation(
        &mut self,
        host: &Host,
        work: &mut WorkspaceCalls,
        operation: w::DeviceOperation,
    ) {
        let refusal = self.refusal(&operation);
        #[cfg(windows)]
        if operation.capability == w::COMPUTER_USE {
            let outcome = match refusal {
                Some(reason) => Err(reason.into()),
                None => Box::pin(self.computer_operation(host, &operation))
                    .await
                    .map(OperationOutput::External),
            };
            let result = Self::operation_result(&self.transport.options.id, &operation, outcome);
            Self::report_operation(host, result).await;
            return;
        }
        if operation.capability.starts_with(work_tools::PREFIX) {
            let workspace = self.devices.workspace.clone();
            let request: Result<_, OperationError> = (|| {
                if let Some(reason) = refusal {
                    return Err(reason.into());
                }
                let workspace = workspace.ok_or(OperationError::NoWorkspace)?;
                Ok((work.admit(&operation)?, workspace))
            })();
            match request {
                Ok((run_id, workspace)) => work.spawn(host, operation, run_id, workspace),
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
            Some(w::notification::Event::InitialState(snapshot)) => {
                let previous = self.transport.restore_snapshot(snapshot);
                restart = previous != self.transport.bindings;
                if restart {
                    if self
                        .audio
                        .speaking_companion
                        .as_deref()
                        .is_some_and(|id| !self.transport.bindings.contains(id))
                    {
                        self.audio.stop_playback();
                        self.audio.speaking_companion = None;
                    }
                    self.devices.sensor = Sensor::default();
                }
                self.sync_avatars().await;
            }
            Some(w::notification::Event::Companion(companion)) => {
                self.transport
                    .companions
                    .insert(companion.id.clone(), (**companion).clone());
                if self.transport.bindings.contains(&companion.id) {
                    self.sync_avatars().await;
                }
                if self.transport.pending.as_deref() == Some(companion.id.as_str()) {
                    restart = true;
                }
            }
            Some(w::notification::Event::CompanionRemoved(companion)) => {
                self.transport.companions.remove(&companion.id);
                if self.transport.pending.as_deref() == Some(companion.id.as_str()) {
                    self.transport.pending = None;
                }
                restart = self.release_binding(&companion.id).await;
            }
            Some(w::notification::Event::Generic(event)) if event.kind == "perception_status" => {
                if self.devices.eyes_enabled
                    && let Some(payload) = event.payload.as_option()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload.json)
                    && value["subscription_id"].as_str()
                        == Some(self.transport.subscription.as_str())
                {
                    let status = value["status"].as_str().map(str::to_owned);
                    super::notify_ui(&self.updates, Update::EyesStatus(status));
                }
            }
            _ => {}
        }
        let _discarded = self
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
        if !ours {
            return self.release_binding(&activation.companion_id).await;
        }
        if !self
            .transport
            .bindings
            .insert(activation.companion_id.clone())
        {
            return false;
        }
        self.transport
            .focused
            .get_or_insert_with(|| activation.companion_id.clone());
        self.sync_avatars().await;
        self.transport.bindings.len() == 1
    }

    async fn release_binding(&mut self, companion_id: &str) -> bool {
        if !self.transport.bindings.remove(companion_id) {
            return false;
        }
        self.audio.clear_speaking_if(companion_id);
        if self.transport.focused.as_deref() == Some(companion_id) {
            self.transport.focused = self.transport.bindings.first().cloned();
        }
        self.sync_avatars().await;
        let last = self.transport.bindings.is_empty();
        if last {
            self.devices.sensor = Sensor::default();
        }
        last
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
