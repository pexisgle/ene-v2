//! The session: the host connection and the devices this client owns.
//!
//! The window never touches a socket or a device. It asks this session to serve
//! a companion, and everything else it does is a single request through the
//! generated clients. The session owns the subscription cursor, the activation
//! generation, the device stream, the audio streams, the avatar child process
//! and the perception sensor, and it reconnects on its own with a bounded
//! backoff.
//!
//! Three rules decide what this module does:
//!
//! - The discovery file is read on every attempt. The host rewrites its port and
//!   token on each start, so a cached endpoint is a dead endpoint.
//! - Operations are executed only from `ClientService.DeviceStream`, and only
//!   when `client_id` and `subscription_id` both still name this client's
//!   current binding. The `device_operation` notification is an observation,
//!   never an instruction.
//! - Nothing is retried blindly. A dropped link reconnects; a request the
//!   operator made is their decision to repeat, so it moves to the window's
//!   "unsent" list with its `request_key` intact.

use crate::{
    audio::{self, Microphone, Player},
    avatar::{self, Avatar},
    capabilities,
    host::{Connect, Discovery, Host},
    perception::{self, NotifyOn, Report, Sensor, Watch},
    state::{Device, Devices, Link},
    update::{Command, Update},
    work_tools::{self, Workspace},
};
use buffa::view::MessageView;
use ene_rpc as w;
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The first wait before a retry, doubled up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
/// How often the client samples the OS for perception.
const SAMPLE: Duration = Duration::from_secs(1);
const SCREEN_SAMPLE: Duration = Duration::from_secs(20);
/// How often captured audio goes to the host.
const CAPTURE: Duration = Duration::from_millis(100);
/// How often the playback amplitude goes to the renderer (about 60 Hz).
const LIP: Duration = Duration::from_millis(16);

/// What the client needs to identify itself and find its renderer.
#[derive(Clone, Debug)]
pub struct Options {
    /// The client id the host stores. Stable across restarts.
    pub id: String,
    /// The display name the host's client list shows.
    pub name: String,
    /// The platform string, for the host's list.
    pub platform: String,
    /// The `ene-avatar` binary, or `None` to run without a renderer.
    pub renderer: Option<PathBuf>,
    /// The VRM model to show.
    pub model: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            id: "desktop".into(),
            name: "Desktop".into(),
            platform: std::env::consts::OS.into(),
            renderer: None,
            model: avatar::default_model(),
        }
    }
}

/// The generated clients over the current connection, which the window reads to
/// send its own one-shot requests. The session replaces the contents whenever it
/// reconnects, so a request can never use a token the host has already rotated.
pub type Connection = Arc<RwLock<Option<Host>>>;

/// The companion this client is the active device for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    companion_id: String,
}

/// Why one attempt at serving the host ended.
enum Outcome {
    Closed,
    Dropped(String),
    Resync(String),
}

/// What one event asks the serve loop to do.
enum Step {
    Continue,
    Restart,
    Stop,
}

pub struct Session {
    path: PathBuf,
    options: Options,
    connection: Connection,
    updates: UnboundedSender<Update>,
    commands: UnboundedReceiver<Command>,
    /// The companion this client serves, and the generation it holds. Exactly
    /// one: the host attributes audio, perception and device results to a client
    /// through `activation_for_client`, so serving two at once would make every
    /// report ambiguous.
    binding: Option<Binding>,
    /// Whether the next attempt may move the activation. It is set on the first
    /// connect and when the operator picks a companion, so a reconnect never
    /// steals the companion back from another device.
    may_activate: bool,
    /// The surface of the current device stream, from `DeviceReady`.
    subscription: String,
    /// The last notification cursor, for a resuming subscription.
    cursor: Option<w::Cursor>,
    avatar: Option<Avatar>,
    player: Option<Player>,
    microphone: Option<Microphone>,
    sensor: Sensor,
    eyes_enabled: bool,
    #[cfg(windows)]
    computer_enabled: bool,
    #[cfg(windows)]
    computer_frame: Option<crate::computer::Frame>,
    /// The companion the operator asked to serve while a request was in flight.
    pending: Option<String>,
    /// Capabilities this machine can honour, declared at registration.
    specs: Vec<w::ClientCapabilitySpec>,
    workspace: Option<Arc<Workspace>>,
    /// The last amplitude sent to the renderer, so silence sends nothing new.
    last_lip: Option<f32>,
    /// Software mute for the microphone.
    mic_muted: bool,
    /// Software mute for speaker output.
    output_muted: bool,
    /// Current avatar display scale percentage.
    avatar_scale: f32,
    /// Last audio state forwarded to the UI, to avoid spamming idle frames.
    last_ui_audio: (f32, f32, bool),
}

/// One validated `body.motion` request, before anything is sent anywhere.
#[derive(Debug, PartialEq)]
struct MotionPlan {
    motion: String,
    /// The clip to show. `idle` can fall back to the rest pose.
    clip: Option<std::path::PathBuf>,
    /// The face to hold, `neutral` to clear it, or nothing to leave it alone.
    expression: Option<String>,
}

impl MotionPlan {
    fn parse(input: &serde_json::Value) -> Result<Self, String> {
        let motion = input
            .get("motion")
            .and_then(serde_json::Value::as_str)
            .ok_or("body.motion requires a `motion` name")?;
        if !avatar::MOTIONS.iter().any(|(preset, _)| *preset == motion) {
            return Err(format!("`{motion}` is not a declared motion preset"));
        }
        let expression = input
            .get("expression")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(name) = &expression
            && !capabilities::EXPRESSIONS.contains(&name.as_str())
        {
            return Err(format!("`{name}` is not a declared expression"));
        }
        let clip = avatar::motion(motion);
        if clip.is_none() && motion != "idle" {
            return Err(format!(
                "the motion asset for `{motion}` is not installed, so nothing was shown"
            ));
        }
        Ok(Self {
            motion: motion.to_owned(),
            clip,
            expression,
        })
    }
}

impl Session {
    pub fn new(
        path: PathBuf,
        options: Options,
        connection: Connection,
        updates: UnboundedSender<Update>,
        commands: UnboundedReceiver<Command>,
    ) -> Self {
        let mut specs =
            capabilities::Declaration::probe(options.renderer.as_deref(), options.model.as_deref())
                .specs();
        let workspace = Workspace::probe().map(Arc::new);
        if let Some(workspace) = &workspace {
            specs.extend(workspace.specs());
        }
        Self {
            path,
            options,
            connection,
            updates,
            commands,
            binding: None,
            may_activate: true,
            subscription: String::new(),
            cursor: None,
            avatar: None,
            player: None,
            microphone: None,
            sensor: Sensor::default(),
            eyes_enabled: perception::enabled(),
            #[cfg(windows)]
            computer_enabled: perception::preference_enabled(crate::computer::PREFERENCE),
            #[cfg(windows)]
            computer_frame: None,
            last_lip: None,
            mic_muted: false,
            output_muted: false,
            avatar_scale: 100.,
            last_ui_audio: (0., 0., false),
            pending: None,
            specs,
            workspace,
        }
    }

    /// Serves until the window closes or the session is asked to quit.
    pub async fn run(mut self) {
        let _ = self.updates.send(Update::EyesEnabled(self.eyes_enabled));
        #[cfg(windows)]
        let _ = self
            .updates
            .send(Update::ComputerUseEnabled(self.computer_enabled));
        let mut retry = FIRST_RETRY;
        loop {
            match self.attempt().await {
                Outcome::Closed => break,
                Outcome::Resync(reason) => {
                    // A subscription the host ended is not a dead link: the
                    // cursor is dropped so the next attempt resnapshots.
                    self.cursor = None;
                    tracing::info!("resubscribing: {reason}");
                    retry = FIRST_RETRY;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Outcome::Dropped(reason) => {
                    let retry_in = retry;
                    retry = (retry * 2).min(MAX_RETRY);
                    tracing::warn!("the link dropped: {reason}");
                    self.report(Link::Lost { reason, retry_in }).await;
                    // The wait is interruptible, so closing the window does not
                    // leave the session asleep for half a minute.
                    tokio::select! {
                        () = tokio::time::sleep(retry_in) => {}
                        command = self.commands.recv() => {
                            match command {
                                Some(Command::SetEyesEnabled(enabled)) => self.set_eyes_enabled(enabled),
                                #[cfg(windows)]
                                Some(Command::SetComputerUseEnabled(enabled)) => self.set_computer_use_enabled(enabled),
                                Some(Command::Quit) | None => break,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        self.shutdown();
    }

    /// One connection attempt: read discovery, register, bind, serve.
    async fn attempt(&mut self) -> Outcome {
        *self.connection.write().unwrap() = None;
        let discovery = match Discovery::read(&self.path) {
            Ok(discovery) => discovery,
            Err(Connect::Missing(path)) => {
                self.report(Link::Unset {
                    path,
                    reason: "ホストがまだ接続情報を公開していません".into(),
                })
                .await;
                return Outcome::Dropped("接続情報がまだありません".into());
            }
            Err(Connect::Invalid(reason)) => {
                self.report(Link::Unusable {
                    reason: reason.clone(),
                })
                .await;
                return Outcome::Dropped(reason);
            }
            Err(error) => return Outcome::Dropped(error.to_string()),
        };
        let host = match Host::connect(&discovery) {
            Ok(host) => host,
            Err(error) => {
                self.report(Link::Unusable {
                    reason: error.to_string(),
                })
                .await;
                return Outcome::Dropped(error.to_string());
            }
        };
        *self.connection.write().unwrap() = Some(host.clone());
        self.report(Link::Connecting {
            endpoint: discovery.endpoint.clone(),
            attempt: 1,
        })
        .await;

        let _record = match self.register(&host).await {
            Ok(record) => record,
            Err(error) => return self.classify(error),
        };
        self.devices().await;

        let snapshot = match host.host.get_snapshot(w::Empty::default()).await {
            Ok(response) => match response.view().to_owned_message() {
                Ok(snapshot) => snapshot,
                Err(error) => return Outcome::Dropped(error.to_string()),
            },
            Err(error) => return self.classify(error),
        };
        let previous = self.binding.clone();
        if let Err(outcome) = self.bind(&host, &snapshot).await {
            return outcome;
        }
        if previous != self.binding {
            self.sensor = Sensor::default();
        }
        self.report(Link::Live {
            version: snapshot.server_version.clone(),
        })
        .await;

        self.open_devices().await;
        self.serve(host).await
    }

    /// Registers this client, keeping the host's grants across restarts.
    async fn register(&self, host: &Host) -> Result<w::Client, connectrpc::ConnectError> {
        // A declaration is an optimistic update, so a restart must send the
        // revision the host already stored or it conflicts with itself.
        let expected_revision = host
            .client
            .list(w::Empty::default())
            .await?
            .view()
            .to_owned_message()
            .ok()
            .and_then(|clients| {
                clients
                    .items
                    .iter()
                    .find(|client| client.id == self.options.id)
                    .map(|client| client.revision)
            })
            .unwrap_or(0);
        let computer_spec = {
            #[cfg(windows)]
            {
                self.computer_enabled.then(crate::computer::spec)
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        let specs: Vec<_> = self
            .specs
            .iter()
            .filter(|spec| self.eyes_enabled || spec.name != "eye.watch")
            .cloned()
            .chain(computer_spec)
            .collect();
        host.client
            .register_client(w::RegisterClientRequest {
                id: self.options.id.clone(),
                name: self.options.name.clone(),
                platform: self.options.platform.clone(),
                capabilities: capabilities::names(&specs),
                capability_specs: specs,
                expected_revision,
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))
    }

    /// Binds this client to the companion it should serve.
    async fn bind(&mut self, host: &Host, snapshot: &w::Snapshot) -> Result<(), Outcome> {
        let wanted = self
            .pending
            .clone()
            .or_else(|| snapshot.active_companion_id.clone())
            .or_else(|| {
                snapshot
                    .companions
                    .first()
                    .map(|companion| companion.id.clone())
            });
        let Some(wanted) = wanted else {
            self.binding = None;
            return Ok(());
        };
        let current = snapshot
            .activations
            .iter()
            .find(|activation| activation.companion_id == wanted)
            .cloned();
        let ours = current
            .as_ref()
            .and_then(|activation| activation.client_id.as_deref())
            == Some(self.options.id.as_str());
        if ours {
            self.binding = current.map(|activation| Binding {
                companion_id: activation.companion_id,
            });
            self.pending = None;
            return Ok(());
        }
        if !self.may_activate {
            // Another device holds this companion. Serving nothing is the
            // honest state: no audio, no perception, no device results.
            self.binding = None;
            self.pending = None;
            tracing::info!("another client is the active device for {wanted}");
            return Ok(());
        }
        match self.activate(host, &wanted).await {
            Ok(binding) => {
                self.binding = Some(binding);
                self.may_activate = false;
                self.pending = None;
                Ok(())
            }
            Err(error) => Err(self.classify(error)),
        }
    }

    /// Moves the activation to this client, releasing a previous companion so
    /// this client is never the active device for two at once.
    async fn activate(
        &mut self,
        host: &Host,
        companion_id: &str,
    ) -> Result<Binding, connectrpc::ConnectError> {
        if let Some(previous) = self.binding.clone()
            && previous.companion_id != companion_id
        {
            // Releasing first keeps the attribution unambiguous; the host then
            // has no active device for the companion this client is leaving.
            self.release(host, &previous).await?;
        }
        let activation = host
            .client
            .activate(w::ActivationRequest {
                companion_id: companion_id.to_owned(),
                client_id: self.options.id.clone(),
                release: false,
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))?;
        tracing::info!(
            companion = %activation.companion_id,
            "the client is the active device"
        );
        Ok(Binding {
            companion_id: activation.companion_id,
        })
    }

    /// Gives up the activation this client holds.
    async fn release(
        &mut self,
        host: &Host,
        binding: &Binding,
    ) -> Result<(), connectrpc::ConnectError> {
        host.client
            .activate(w::ActivationRequest {
                companion_id: binding.companion_id.clone(),
                client_id: String::new(),
                release: true,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Serves the device stream, audio, the microphone and the subscription.
    async fn serve(&mut self, host: Host) -> Outcome {
        self.subscription.clear();
        #[cfg(windows)]
        {
            self.computer_frame = None;
        }
        let _ = self.updates.send(Update::EyesStatus(None));
        if self.options.renderer.is_some() && self.avatar.is_none() {
            self.open_avatar().await;
        }
        let mut device = match host
            .client
            .device_stream(w::Id {
                id: self.options.id.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(stream) => stream,
            Err(error) => return self.classify(error),
        };
        // The audio subscription is what gives this client the speech floor, so
        // it is opened only while this client is the active device.
        let mut output = match self.binding.clone() {
            Some(_) => {
                match host
                    .voice
                    .subscribe_audio_output(w::SubscribeAudioRequest {
                        client_id: self.options.id.clone(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(stream) => Some(stream),
                    Err(error) => {
                        tracing::info!("no audio output stream: {error}");
                        self.set_out(Device::Unavailable(error.to_string())).await;
                        None
                    }
                }
            }
            None => None,
        };
        let (mut mic_send, mut mic_recv) = match self.binding.clone() {
            Some(_) => {
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
            None => (None, None),
        };
        let mut mic_retry_at: Option<tokio::time::Instant> = None;
        let mut mic_error_reported = false;
        let mut subscription = match self.subscribe(&host).await {
            Ok(stream) => stream,
            Err(outcome) => return outcome,
        };
        let mut sample = interval(SAMPLE);
        let mut screen_at = tokio::time::Instant::now();
        let mut eyes_open = false;
        let mut observations = tokio::task::JoinSet::new();
        let mut capture = interval(CAPTURE);
        let mut lip = interval(LIP);
        let mut work = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                frame = device.message() => match frame {
                    Ok(Some(message)) => {
                        let event = message.to_owned_message();
                        {
                            if let Some(event) = event.event {
                                match self.device_event(&host, event, &mut work).await {
                                    Step::Continue => {}
                                    Step::Restart => return Outcome::Resync("the binding changed".into()),
                                    Step::Stop => return Outcome::Closed,
                                }
                            }
                        }
                    }
                    Ok(None) => return Outcome::Dropped("the host closed the device stream".into()),
                    Err(error) => return self.classify(error),
                },
                Some(result) = work.join_next(), if !work.is_empty() => {
                    match result {
                        Ok(result) => Self::report_operation(&host, result).await,
                        Err(error) => tracing::warn!("workspace operation did not finish: {error}"),
                    }
                },
                frame = audio_message(&mut output) => match frame {
                    Some(Ok(event)) => self.play(event).await,
                    Some(Err(error)) => {
                        // A host without TTS answers `unimplemented` or
                        // `failed_precondition`; that is a client without audio,
                        // not a broken link.
                        if persistent(&error) {
                            tracing::info!("the host has no audio output: {error}");
                            self.set_out(Device::Unavailable(error.to_string())).await;
                        } else {
                            return self.classify(error);
                        }
                        output = None;
                    }
                    None => output = None,
                },
                frame = transcription_message(&mut mic_recv) => match frame {
                    Some(Ok(event)) => {
                        mic_error_reported = false;
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
                            mic_retry_at = None;
                        } else {
                            tracing::warn!("the transcription stream ended: {error}");
                            if !mic_error_reported {
                                mic_error_reported = true;
                                let _ = self.updates.send(Update::Failed {
                                    context: "音声認識",
                                    message: error.to_string(),
                                });
                            }
                            if self.binding.is_some() && self.microphone.is_some() {
                                mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                            }
                        }
                        mic_recv = None;
                        mic_send = None;
                    }
                    None => {
                        let _ = self.updates.send(Update::Transcription {
                            text: String::new(),
                            is_final: true,
                            is_speaking: false,
                        });
                        if self.binding.is_some() && self.microphone.is_some() {
                            mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                        }
                        mic_recv = None;
                        mic_send = None;
                    }
                },
                () = retry_wait(mic_retry_at) => {
                    mic_retry_at = None;
                    if self.binding.is_some() && mic_recv.is_none() {
                        match host.voice.stream_audio_input().await {
                            Ok(stream) => {
                                let (send, recv) = stream.into_split();
                                mic_send = Some(send);
                                mic_recv = Some(recv);
                                mic_error_reported = false;
                            }
                            Err(error) => {
                                if !persistent(&error) {
                                    mic_retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(10));
                                }
                            }
                        }
                    }
                },
                _ = capture.tick() => {
                    let speaking = self.player.as_ref().is_some_and(|player| player.playing() && !player.is_muted());
                    let CaptureTick { chunks, state } =
                        capture_tick(&mut self.microphone, &self.binding, &self.options.id, speaking);
                    let mut sent = 0;
                    if let Some(stream) = mic_send.as_mut() {
                        for chunk in chunks {
                            let bytes = chunk.pcm_data.len();
                            if let Err(error) = stream.send(chunk).await {
                                tracing::info!("the microphone stream closed: {error}");
                                mic_send = None;
                                break;
                            }
                            sent += bytes;
                        }
                    }
                    let _ = self.updates.send(Update::Capture {
                        level: state.level,
                        open: state.open,
                        sent,
                    });
                }
                _ = lip.tick() => {
                    self.drain_renderer();
                    self.pump_lip();
                }
                _ = sample.tick(), if observations.is_empty() => {
                    self.sample(&host, &mut screen_at, &mut eyes_open, &mut observations);
                }
                _ = observations.join_next(), if !observations.is_empty() => {}
                frame = subscription.message() => match frame {
                    Ok(Some(message)) => {
                        let notification = message.to_owned_message();
                        match self.notification(notification).await {
                            Step::Continue => {}
                            Step::Restart => return Outcome::Resync("resync requested".into()),
                            Step::Stop => return Outcome::Closed,
                        }
                    }
                    Ok(None) => return Outcome::Resync("the host closed the subscription".into()),
                    Err(error) => return self.classify(error),
                },
                Some(command) = self.commands.recv() => {
                    if matches!(command, Command::SetEyesEnabled(false)) {
                        observations.abort_all();
                        if eyes_open {
                            let event = self.perception_event(w::perception_event::Event::Enabled(false));
                            let _ = tokio::time::timeout(Duration::from_secs(1), host.client.report_perception(event)).await;
                        }
                    }
                    match self.command(&host, command).await {
                        Step::Continue => {}
                        Step::Restart => return Outcome::Resync("the session settings changed".into()),
                        Step::Stop => return Outcome::Closed,
                    }
                },
                else => return Outcome::Closed,
            }
        }
    }

    /// Opens the notification subscription, resuming from the last cursor.
    async fn subscribe(
        &mut self,
        host: &Host,
    ) -> Result<
        connectrpc::client::ServerStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            w::__buffa::view::NotificationView<'static>,
        >,
        Outcome,
    > {
        match host
            .host
            .subscribe(w::SubscribeRequest {
                client_id: self.options.id.clone(),
                cursor: self.cursor.clone().into(),
                ..Default::default()
            })
            .await
        {
            Ok(stream) => Ok(stream),
            Err(error) => Err(self.classify(error)),
        }
    }

    /// Handles one device event, executing only from the device stream.
    async fn device_event(
        &mut self,
        host: &Host,
        event: w::device_event::Event,
        work: &mut tokio::task::JoinSet<w::DeviceOperationResult>,
    ) -> Step {
        match event {
            w::device_event::Event::Ready(ready) => {
                self.subscription = ready.subscription_id;
                Step::Continue
            }
            w::device_event::Event::Operation(operation) => {
                let refusal = self.refusal(&operation);
                #[cfg(windows)]
                if operation.capability == w::COMPUTER_USE {
                    let outcome = match refusal {
                        Some(reason) => Err(reason),
                        None => self.computer_operation(host, &operation).await,
                    };
                    let result = Self::operation_result(&self.options.id, &operation, outcome);
                    Self::report_operation(host, result).await;
                    return Step::Continue;
                }
                if operation.capability.starts_with(work_tools::PREFIX) {
                    let workspace = self.workspace.clone();
                    let request = (|| {
                        if let Some(reason) = refusal {
                            return Err(reason);
                        }
                        if operation.work_id.as_deref().is_none_or(str::is_empty) {
                            return Err("workspace operations require a work session".into());
                        }
                        if work.len() >= 8 {
                            return Err("the workspace is busy".into());
                        }
                        Ok((
                            workspace.ok_or("no workspace is configured")?,
                            Self::operation_input(&operation)?,
                        ))
                    })();
                    match request {
                        Ok((workspace, input)) => {
                            let client_id = self.options.id.clone();
                            work.spawn(async move {
                                let output = workspace.execute(&operation.capability, input).await;
                                Self::operation_result(&client_id, &operation, output)
                            });
                        }
                        Err(reason) => {
                            let result =
                                Self::operation_result(&self.options.id, &operation, Err(reason));
                            Self::report_operation(host, result).await;
                        }
                    }
                } else {
                    let outcome = match refusal {
                        Some(reason) => Err(reason),
                        None => self.execute(&operation),
                    };
                    let result = Self::operation_result(&self.options.id, &operation, outcome);
                    Self::report_operation(host, result).await;
                }
                Step::Continue
            }
        }
    }

    async fn report_operation(host: &Host, result: w::DeviceOperationResult) {
        let operation_id = result.operation_id.clone();
        match host.client.complete_operation(result).await {
            Ok(response) => {
                if let Ok(state) = response.view().to_owned_message()
                    && state.status == w::DeviceOperationStatus::Uncertain
                {
                    tracing::warn!(operation = %operation_id, "the host could not confirm the operation result");
                }
            }
            Err(error) => {
                tracing::warn!(operation = %operation_id, "the operation result was not reported: {error}")
            }
        }
    }

    #[cfg(windows)]
    async fn computer_operation(
        &mut self,
        host: &Host,
        operation: &w::DeviceOperation,
    ) -> Result<serde_json::Value, String> {
        if !self.computer_enabled || self.subscription.is_empty() {
            return Err("Computer Use is disabled or the device stream is not ready".into());
        }
        let work_id = operation
            .work_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or("Computer Use requires a work session")?;
        let run_id = operation
            .run_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or("Computer Use requires a bound run")?;
        let (work, state) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::try_join!(
                host.work.get_work(w::WorkRequest {
                    work_id: work_id.to_owned(),
                    ..Default::default()
                }),
                host.client.get_operation(w::Id {
                    id: operation.operation_id.clone(),
                    ..Default::default()
                }),
            )
        })
        .await
        .map_err(|_| "could not verify the active run in time")?
        .map_err(|_| "could not verify the active run")?;
        let work = work.view();
        let session = work
            .session
            .as_option()
            .ok_or("the work session is unavailable")?;
        if session.client_id != self.options.id
            || self
                .binding
                .as_ref()
                .is_none_or(|binding| binding.companion_id != session.companion_id)
            || !work
                .runs
                .iter()
                .any(|run| run.run_id == run_id && run.state == w::RunStatus::Running)
            || state.view().status != w::DeviceOperationStatus::Dispatched
        {
            return Err("the Computer Use operation is no longer active on this device".into());
        }
        let input = Self::operation_input(operation)?;
        crate::computer::execute(
            &mut self.computer_frame,
            run_id,
            input,
            self.avatar.as_ref().map(Avatar::pid),
        )
        .await
    }

    fn operation_result(
        client_id: &str,
        operation: &w::DeviceOperation,
        outcome: Result<serde_json::Value, String>,
    ) -> w::DeviceOperationResult {
        match &outcome {
            Ok(_) => tracing::info!(
                operation = %operation.operation_id,
                capability = %operation.capability,
                "device operation applied"
            ),
            Err(reason) => tracing::warn!(
                operation = %operation.operation_id,
                capability = %operation.capability,
                reason = %reason,
                "device operation not applied"
            ),
        }
        let (success, output, failure) = match outcome {
            Ok(output) => (true, output.to_string(), None),
            Err(reason) => (
                false,
                String::new(),
                Some(w::Failure {
                    code: {
                        #[cfg(windows)]
                        if operation.capability == w::COMPUTER_USE
                            && reason == crate::computer::PARTIAL_INPUT
                        {
                            "operation_uncertain"
                        } else {
                            "operation_failed"
                        }
                        #[cfg(not(windows))]
                        "operation_failed"
                    }
                    .into(),
                    message: reason,
                    ..Default::default()
                }),
            ),
        };
        w::DeviceOperationResult {
            client_id: client_id.to_owned(),
            operation_id: operation.operation_id.clone(),
            success,
            output,
            failure: failure.into(),
            ..Default::default()
        }
    }

    /// Why this client must not execute the operation, if it must not.
    fn refusal(&self, operation: &w::DeviceOperation) -> Option<String> {
        let claim = Claim {
            client_id: &operation.client_id,
            subscription_id: &operation.subscription_id,
        };
        admits(
            &self.options.id,
            &self.subscription,
            self.binding.as_ref(),
            &claim,
        )
        .err()
    }

    fn operation_input(operation: &w::DeviceOperation) -> Result<serde_json::Value, String> {
        if operation.input.trim().is_empty() {
            Ok(serde_json::json!({}))
        } else {
            serde_json::from_str(&operation.input)
                .map_err(|error| format!("the operation input is not JSON: {error}"))
        }
    }

    /// Runs an avatar or perception capability on the session thread.
    fn execute(&mut self, operation: &w::DeviceOperation) -> Result<serde_json::Value, String> {
        let input = Self::operation_input(operation)?;
        match operation.capability.as_str() {
            "body.motion" => self.motion(&input),
            "eye.watch" => self.watch(&input),
            other => Err(format!("this client does not implement `{other}`")),
        }
    }

    /// Shows a motion. Without a renderer the operation fails; it did not happen.
    fn motion(&mut self, input: &serde_json::Value) -> Result<serde_json::Value, String> {
        let plan = MotionPlan::parse(input)?;
        let avatar = self
            .avatar
            .as_mut()
            .ok_or("no renderer is configured, so no motion was shown")?;
        let model = self
            .options
            .model
            .as_ref()
            .ok_or("no model is configured for the renderer")?;
        if avatar.show(
            &self.options.id,
            model,
            plan.clip.as_deref(),
            plan.motion == "idle",
        )? {
            avatar.await_state(&self.options.id, Duration::from_secs(2))?;
        }
        // A face is separate from the motion: `neutral` clears it, and the
        // renderer keeps it across later motion changes.
        if let Some(name) = &plan.expression {
            let face = (name != "neutral").then_some(name.as_str());
            avatar.expression(&self.options.id, face)?;
        }
        Ok(serde_json::json!({
            "applied": true,
            "motion": plan.motion,
            "expression": plan.expression,
        }))
    }

    /// Sends the amplitude the device last played, so the renderer can open the
    /// mouth in step with the audio. A tick at the audio rate is worthless once
    /// the level stops changing, so repeats of the last value are dropped.
    fn pump_lip(&mut self) {
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
        if self
            .last_lip
            .is_some_and(|last| (last - level).abs() < 0.005)
        {
            return;
        }
        let id = self.options.id.clone();
        let Some(avatar) = self.avatar.as_mut() else {
            return;
        };
        if avatar.lip_sync(&id, level).is_ok() {
            tracing::trace!("lip sync {level:.3}");
            self.last_lip = Some(level);
        }
    }

    /// Applies or clears a watch rule.
    fn watch(&mut self, input: &serde_json::Value) -> Result<serde_json::Value, String> {
        if !self.eyes_enabled {
            return Err("「目」はオフです".into());
        }
        let watch = Watch::parse(input)?;
        let enabled = watch.enabled;
        self.sensor.apply(if enabled { Some(watch) } else { None });
        Ok(serde_json::json!({"applied": true, "enabled": enabled}))
    }

    /// Handles one host notification: forwards it, and tracks the cursor and the
    /// activation this client holds.
    async fn notification(&mut self, notification: w::Notification) -> Step {
        self.cursor = notification.cursor.as_option().cloned();
        let mut restart = false;
        match notification.event.as_ref() {
            Some(w::notification::Event::ResyncRequired(_)) => {
                return Step::Restart;
            }
            Some(w::notification::Event::ActivationUpdated(activation)) => {
                if self
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.companion_id == activation.companion_id)
                {
                    let ours = activation.client_id.as_deref() == Some(self.options.id.as_str());
                    if !ours {
                        // Another device holds this companion now. Old audio and
                        // old streams must not be reused for it.
                        self.binding = None;
                        self.stop_playback();
                        self.sensor = Sensor::default();
                        restart = true;
                    }
                }
            }
            Some(w::notification::Event::Companion(companion))
                if self.pending.as_deref() == Some(companion.id.as_str()) =>
            {
                restart = true;
            }
            Some(w::notification::Event::Generic(event)) if event.kind == "perception_status" => {
                if self.eyes_enabled
                    && let Some(payload) = event.payload.as_option()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload.json)
                    && value["subscription_id"].as_str() == Some(self.subscription.as_str())
                {
                    let status = value["status"].as_str().map(str::to_owned);
                    let _ = self.updates.send(Update::EyesStatus(status));
                }
            }
            _ => {}
        }
        let _ = self
            .updates
            .send(Update::Notification(Box::new(notification)));
        if restart {
            Step::Restart
        } else {
            Step::Continue
        }
    }

    /// Handles one window command.
    async fn command(&mut self, host: &Host, command: Command) -> Step {
        match command {
            Command::Serve(companion_id) => {
                self.pending = Some(companion_id);
                self.may_activate = true;
                Step::Restart
            }
            Command::StopPlayback => {
                self.stop_playback();
                Step::Continue
            }
            Command::Refresh => Step::Restart,
            Command::SetEyesEnabled(enabled) => {
                self.set_eyes_enabled(enabled);
                Step::Restart
            }
            #[cfg(windows)]
            Command::SetComputerUseEnabled(enabled) => {
                self.set_computer_use_enabled(enabled);
                Step::Restart
            }
            Command::SetMicMuted(muted) => {
                self.mic_muted = muted;
                if let Some(microphone) = self.microphone.as_ref() {
                    microphone.set_muted(muted);
                }
                let _ = self.updates.send(Update::AudioMute {
                    mic_muted: self.mic_muted,
                    output_muted: self.output_muted,
                });
                Step::Continue
            }
            Command::SetOutputMuted(muted) => {
                self.output_muted = muted;
                if let Some(player) = self.player.as_ref() {
                    player.set_muted(muted);
                }
                let _ = self.updates.send(Update::AudioMute {
                    mic_muted: self.mic_muted,
                    output_muted: self.output_muted,
                });
                Step::Continue
            }
            Command::SetAvatarScale(scale) => {
                let scale = scale.clamp(50., 200.);
                self.avatar_scale = scale;
                if let (Some(avatar), Some(model)) =
                    (self.avatar.as_mut(), self.options.model.as_ref())
                {
                    if let Err(error) = avatar.set_scale(&self.options.id, model, scale) {
                        let _ = self.updates.send(Update::Failed {
                            context: "アバターのサイズ変更",
                            message: error,
                        });
                    } else {
                        let _ = self.updates.send(Update::AvatarScaleChanged(scale));
                        self.devices().await;
                    }
                } else {
                    let _ = self.updates.send(Update::Failed {
                        context: "アバターのサイズ変更",
                        message: "アバター描画プロセスまたはモデルが設定されていません".into(),
                    });
                }
                Step::Continue
            }
            Command::Speak(text) => {
                let host = host.clone();
                let updates = self.updates.clone();
                let client_id = self.options.id.clone();
                // Synthesis is a network call; awaiting it here would stall the
                // microphone and the playback this loop owns.
                tokio::spawn(async move {
                    let update = match host
                        .voice
                        .speak(w::SpeakRequest {
                            client_id,
                            text,
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(_) => Update::Spoke,
                        Err(error) => Update::SpokeFailed {
                            message: error.to_string(),
                        },
                    };
                    let _ = updates.send(update);
                });
                Step::Continue
            }
            Command::CenterAvatar => {
                if let Some(avatar) = self.avatar.as_mut() {
                    if let Err(error) = avatar.center(&self.options.id) {
                        let _ = self.updates.send(Update::Failed {
                            context: "アバターの中央移動",
                            message: error,
                        });
                    }
                } else {
                    let _ = self.updates.send(Update::Failed {
                        context: "アバターの中央移動",
                        message: "アバター描画プロセスが起動していません".into(),
                    });
                }
                Step::Continue
            }
            Command::Quit => Step::Stop,
        }
    }

    /// Stops playback now.
    fn stop_playback(&mut self) {
        if let Some(player) = self.player.as_mut() {
            player.flush(u64::MAX);
        }
    }

    /// Opens the avatar renderer, reporting why it is unavailable when it is.
    async fn open_avatar(&mut self) {
        let Some(program) = self.options.renderer.clone() else {
            return;
        };
        match Avatar::spawn(&program) {
            Ok(mut avatar) => {
                if let Some(model) = self.options.model.as_ref() {
                    let initial_motion = avatar::motion("idle");
                    if let Err(error) =
                        avatar.show(&self.options.id, model, initial_motion.as_deref(), true)
                    {
                        tracing::warn!("the renderer did not show the model: {error}");
                    } else if (self.avatar_scale - 100.).abs() > f32::EPSILON
                        && let Err(error) =
                            avatar.set_scale(&self.options.id, model, self.avatar_scale)
                    {
                        tracing::warn!("the renderer did not apply scale: {error}");
                    }
                }
                self.avatar = Some(avatar);
                self.devices().await;
            }
            Err(error) => {
                tracing::warn!("the renderer did not start: {error}");
                self.set_avatar(Device::Unavailable(error)).await;
            }
        }
    }

    /// Opens the output device once.
    async fn open_devices(&mut self) {
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
                    self.set_out(Device::Unavailable(error)).await;
                    return;
                }
            }
        }
        self.devices().await;
    }

    async fn open_microphone(&mut self) {
        if self.microphone.is_some() {
            self.devices().await;
            return;
        }
        match Microphone::open() {
            Ok(microphone) => {
                microphone.set_muted(self.mic_muted);
                self.microphone = Some(microphone);
                self.devices().await;
            }
            Err(error) => {
                tracing::warn!("no microphone: {error}");
                self.set_in(Device::Unavailable(error)).await;
            }
        }
    }

    /// Plays one audio event. Only one utterance is audible at a time.
    async fn play(&mut self, event: w::audio_output_event::Event) {
        match event {
            w::audio_output_event::Event::Chunk(chunk) => {
                tracing::debug!(
                    utterance = %chunk.utterance_id,
                    generation = chunk.generation_id,
                    rate = chunk.sample_rate,
                    "audio output chunk"
                );
                let Some(player) = self.player.as_mut() else {
                    return;
                };
                if let Err(error) =
                    player.push(&chunk.utterance_id, chunk.generation_id, &chunk.pcm_data)
                {
                    tracing::warn!("the audio chunk was not played: {error}");
                }
            }
            w::audio_output_event::Event::Flush(flush) => {
                if let Some(player) = self.player.as_mut() {
                    player.flush(flush.flush_generation_id);
                }
                tracing::info!(reason = ?flush.reason, "the host flushed playback");
            }
            w::audio_output_event::Event::Finished(finished) => {
                tracing::debug!(
                    utterance = %finished.utterance_id,
                    sentences = finished.total_sentences,
                    "the host finished sending an utterance"
                );
            }
        }
    }

    /// Returns to idle when the renderer finishes a gesture.
    fn drain_renderer(&mut self) {
        let Some(avatar) = self.avatar.as_mut() else {
            return;
        };
        for event in avatar.notifications() {
            tracing::debug!(?event, "renderer");
            if event["event"] == "finished"
                && event["character"] == self.options.id.as_str()
                && let Some(model) = self.options.model.as_deref()
                && let Err(error) = avatar.return_to_idle(&self.options.id, model)
            {
                tracing::warn!(%error, "the renderer could not return to idle");
            }
        }
    }

    fn set_eyes_enabled(&mut self, enabled: bool) {
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
    fn set_computer_use_enabled(&mut self, enabled: bool) {
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

    fn perception_event(&self, event: w::perception_event::Event) -> w::PerceptionEvent {
        w::PerceptionEvent {
            client_id: self.options.id.clone(),
            subscription_id: self.subscription.clone(),
            event: Some(event),
            ..Default::default()
        }
    }

    /// Keeps socket waits and screenshot encoding off the audio/device loop.
    fn sample(
        &mut self,
        host: &Host,
        screen_at: &mut tokio::time::Instant,
        eyes_open: &mut bool,
        observations: &mut tokio::task::JoinSet<()>,
    ) {
        use w::perception_event::Event;
        if !self.eyes_enabled
            || self.binding.is_none()
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
        events.extend(reports.into_iter().map(|report| {
            match report {
                Report::Window(window) => Event::ActiveWindow(Box::new(window_report(window))),
                Report::Interrupt(window, transition) => {
                    Event::WatchMatch(Box::new(w::WatchMatch {
                        window: Some(window_report(window)).into(),
                        transition: match transition {
                            NotifyOn::Focus => w::WatchTransition::Focus,
                            NotifyOn::Blur => w::WatchTransition::Blur,
                            NotifyOn::TitleChange => w::WatchTransition::TitleChange,
                        }
                        .into(),
                        ..Default::default()
                    }))
                }
                Report::Activity {
                    is_active,
                    idle_seconds,
                } => Event::UserActivity(Box::new(w::UserActivityState {
                    is_active,
                    idle_seconds,
                    ..Default::default()
                })),
            }
        }));
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

    /// Reports the link state.
    async fn report(&self, link: Link) {
        let _ = self.updates.send(Update::Link(Box::new(link)));
    }

    /// Replaces the output device in the footer.
    async fn set_out(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: self.avatar_state(),
            output: state,
            input: self.input_state(),
        })));
    }

    async fn set_in(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: self.avatar_state(),
            output: self.output_state(),
            input: state,
        })));
    }

    async fn set_avatar(&mut self, state: Device) {
        let _ = self.updates.send(Update::Devices(Box::new(Devices {
            avatar: state,
            output: self.output_state(),
            input: self.input_state(),
        })));
    }

    /// Sends the current device states.
    async fn devices(&mut self) {
        let devices = Devices {
            avatar: self.avatar_state(),
            output: self.output_state(),
            input: self.input_state(),
        };
        let _ = self.updates.send(Update::Devices(Box::new(devices)));
    }

    fn avatar_state(&self) -> Device {
        match self.avatar.as_ref() {
            Some(avatar) => Device::Ready(
                avatar
                    .describe(&self.options.id)
                    .unwrap_or_else(|| "描画中".into()),
            ),
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

    /// Turns an RPC error into what the window must show.
    fn classify(&self, error: connectrpc::ConnectError) -> Outcome {
        match error.code {
            // The endpoint or token is stale: the next attempt re-reads the file.
            connectrpc::ErrorCode::Unauthenticated | connectrpc::ErrorCode::PermissionDenied => {
                Outcome::Dropped(format!(
                    "認証が拒否されました。ホストの接続情報を読み直します: {error}"
                ))
            }
            _ => Outcome::Dropped(error.to_string()),
        }
    }

    /// Stops the renderer. Dropping the child kills it, so this is the whole of
    /// the client's process ownership.
    fn shutdown(&mut self) {
        if let Some(avatar) = self.avatar.take() {
            avatar.shutdown();
        }
    }
}

/// A periodic timer that does not try to catch up after a stall.
fn window_report(window: perception::Window) -> w::ActiveWindowChanged {
    w::ActiveWindowChanged {
        title: window.title,
        app_id: window.app_id,
        ..Default::default()
    }
}

fn interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Whether an error means the host has no such feature, rather than a broken link.
fn persistent(error: &connectrpc::ConnectError) -> bool {
    matches!(
        error.code,
        connectrpc::ErrorCode::Unimplemented | connectrpc::ErrorCode::FailedPrecondition
    )
}

/// One frame from the audio stream. An absent stream parks this branch forever
/// instead of returning at once, which would spin a core.
async fn audio_message(
    output: &mut Option<
        connectrpc::client::ServerStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            w::__buffa::view::AudioOutputEventView<'static>,
        >,
    >,
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
    input: &mut Option<
        connectrpc::client::BidiRecvHalf<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            w::__buffa::view::TranscriptionEventView<'static>,
        >,
    >,
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
}

#[derive(Clone, Copy, Default)]
struct CaptureState {
    level: f32,
    open: bool,
}

/// Takes ordered PCM chunks and end markers from the microphone.
fn capture_tick(
    microphone: &mut Option<Microphone>,
    binding: &Option<Binding>,
    client_id: &str,
    speaking: bool,
) -> CaptureTick {
    let Some(microphone) = microphone.as_mut() else {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState::default(),
        };
    };
    // The level is read before the gate so the probe shows the microphone even
    // while the gate is closed; that is the difference the test screen exists to
    // make visible.
    let level = microphone.level();
    if binding.is_none() {
        return CaptureTick {
            chunks: Vec::new(),
            state: CaptureState { level, open: false },
        };
    }
    let output = microphone.take(speaking);
    let state = CaptureState {
        level,
        open: output.open,
    };
    let chunks = output
        .chunks
        .into_iter()
        .map(|pcm_data| w::AudioInputChunk {
            client_id: client_id.to_owned(),
            pcm_data,
            sample_rate: audio::WIRE_RATE,
            ..Default::default()
        })
        .collect();
    CaptureTick { chunks, state }
}

/// What one operation claims about the client it was sent to.
struct Claim<'a> {
    client_id: &'a str,
    subscription_id: &'a str,
}

/// Whether this client may execute the operation, or why it must not.
///
/// An operation is executable only when both parts of the claim still name this
/// client's own identity and its current device stream, and this client is the
/// active device for some companion. A client that executed a stale claim would
/// move the wrong character, or report a result the host attributes to another
/// device's stream. The tests below enumerate every combination of the fields,
/// so this decision is checked in full rather than sampled.
fn admits(
    our_id: &str,
    our_subscription: &str,
    binding: Option<&Binding>,
    claim: &Claim<'_>,
) -> Result<(), String> {
    if claim.client_id != our_id {
        return Err(format!(
            "this operation is addressed to `{}`",
            claim.client_id
        ));
    }
    if !our_subscription.is_empty() && claim.subscription_id != our_subscription {
        return Err("this operation belongs to an older device stream".into());
    }
    match binding {
        Some(_) => Ok(()),
        None => Err("this client is not the active device for any companion".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held() -> Binding {
        Binding {
            companion_id: "companion".into(),
        }
    }

    /// Every case the guard can distinguish: our own identity, our own
    /// subscription, and whether we hold the companion, each against every
    /// operation claim the host could send.
    #[test]
    fn only_the_claim_naming_the_current_binding_executes() {
        let our_ids = ["desktop", ""];
        let our_subscriptions = ["sub-1", ""];
        let claim_ids = ["desktop", "other", ""];
        let claim_subscriptions = ["sub-1", "sub-2", ""];
        let held_bindings = [false, true];

        let mut cases = 0usize;
        for our_id in our_ids {
            for our_subscription in our_subscriptions {
                for claim_id in claim_ids {
                    for claim_subscription in claim_subscriptions {
                        for holds in held_bindings {
                            let binding = holds.then(held);
                            let claim = Claim {
                                client_id: claim_id,
                                subscription_id: claim_subscription,
                            };
                            // Stated independently of `admits`.
                            let expected = claim_id == our_id
                                && (our_subscription.is_empty()
                                    || claim_subscription == our_subscription)
                                && holds;
                            let allowed =
                                admits(our_id, our_subscription, binding.as_ref(), &claim);
                            assert_eq!(
                                allowed.is_ok(),
                                expected,
                                "id {our_id:?}/{claim_id:?} subscription {our_subscription:?}/\
{claim_subscription:?} holds {holds:?}: {allowed:?}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2 * 2 * 3 * 3 * 2, "the domain changed size");
    }

    #[test]
    fn a_refusal_says_which_part_does_not_match() {
        let binding = held();
        let stale = Claim {
            client_id: "desktop",
            subscription_id: "sub-1",
        };
        let refused = admits("desktop", "sub-2", Some(&binding), &stale).unwrap_err();
        assert!(refused.contains("older device stream"), "{refused}");

        let elsewhere = Claim {
            client_id: "desk-1",
            subscription_id: "sub-1",
        };
        let refused = admits("desktop", "sub-1", Some(&binding), &elsewhere).unwrap_err();
        assert!(refused.contains("desk-1"), "{refused}");

        let unbound = admits("desktop", "sub-1", None, &stale).unwrap_err();
        assert!(unbound.contains("not the active device"), "{unbound}");
    }
    #[test]
    fn a_motion_plan_carries_the_face_with_the_motion() {
        let plan = MotionPlan::parse(&serde_json::json!({"motion": "idle"})).unwrap();
        assert_eq!(plan.motion, "idle");
        assert_eq!(plan.expression, None);
        assert_eq!(plan.clip, avatar::motion("idle"));

        let plan = MotionPlan::parse(&serde_json::json!({"motion": "idle", "expression": "happy"}))
            .unwrap();
        assert_eq!(plan.expression.as_deref(), Some("happy"));

        // `neutral` is a real request, not an absent one: it clears the face.
        let plan =
            MotionPlan::parse(&serde_json::json!({"motion": "idle", "expression": "neutral"}))
                .unwrap();
        assert_eq!(plan.expression.as_deref(), Some("neutral"));

        // An explicit null leaves the face alone, like an absent field.
        let plan =
            MotionPlan::parse(&serde_json::json!({"motion": "idle", "expression": null})).unwrap();
        assert_eq!(plan.expression, None);
    }

    #[test]
    fn a_motion_plan_rejects_what_this_client_never_declared() {
        let error = MotionPlan::parse(&serde_json::json!({"motion": "idle", "expression": "smug"}))
            .unwrap_err();
        assert!(error.contains("not a declared expression"), "{error}");
        let error = MotionPlan::parse(&serde_json::json!({"motion": "dance"})).unwrap_err();
        assert!(error.contains("not a declared motion preset"), "{error}");
        let error = MotionPlan::parse(&serde_json::json!({})).unwrap_err();
        assert!(error.contains("requires a `motion`"), "{error}");
    }
}
