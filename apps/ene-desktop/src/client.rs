//! The client's lifecycle against the host.
//!
//! One registration, one activation, one device stream. Every operation the
//! host dispatches is applied here and answered with the generation it was
//! accepted for; a result for a generation the host has moved past is refused
//! by the host, so this side never has to guess.

use crate::{
    audio::{self, Microphone, Player},
    avatar::{self, Avatar},
    capabilities,
    host::{Connect, Host},
    perception::{self, Report, Sensor, Watch},
};
use buffa::view::MessageView;
use ene_rpc as w;
use serde_json::{Value, json};
use std::path::PathBuf;
/// What the client needs to identify itself and find its renderer.
#[derive(Clone, Debug)]
pub struct Options {
    /// The client id the host stores. Stable across restarts.
    pub id: String,
    /// The display name shown in the host's client list.
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

/// The activation the client is currently bound to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activation {
    pub companion_id: String,
    pub revision: u64,
}

/// The running client. It owns the renderer, so it is not `Clone`.
pub struct Client {
    host: Host,
    options: Options,
    activation: Activation,
    /// The client record the host stored, including its revision.
    record: Option<w::Client>,
    avatar: Option<Avatar>,
    sensor: Sensor,
    /// The output device. Playback is optional: a machine with no output
    /// device still serves motions and perception.
    player: Option<Player>,
    /// The utterance currently being played, so a new one replaces it.
    playing_utterance: Option<String>,
    /// The input device, opened only when the host accepts microphone audio.
    microphone: Option<Microphone>,
}

impl Client {
    /// Connects, registers and opens the device stream.
    pub async fn start(host: Host, options: Options) -> Result<Self, Connect> {
        let mut client = Self {
            host,
            options,
            activation: Activation::default(),
            record: None,
            avatar: None,
            sensor: Sensor::default(),
            player: None,
            playing_utterance: None,
            microphone: None,
        };
        client.register().await?;
        client.open_avatar().map_err(Connect::Unavailable)?;
        client.open_audio();
        client.refresh_activation().await?;
        Ok(client)
    }

    /// Opens the output device. A machine without one is a degraded client,
    /// not a failed start: motions and perception still work.
    fn open_audio(&mut self) {
        match Player::open() {
            Ok(player) => {
                tracing::info!(
                    rate = player.rate(),
                    channels = player.channels(),
                    "the output device is open"
                );
                self.player = Some(player);
            }
            Err(error) => tracing::warn!("no audio output: {error}"),
        }
    }

    /// Declares this client's capabilities. A re-registration keeps the host's
    /// grants: the client never sends them.
    ///
    /// The host treats a declaration as an optimistic update, so a client that
    /// restarts must send the revision the host already stored. Reading it
    /// first makes a restart idempotent instead of an `aborted` conflict.
    async fn register(&mut self) -> Result<(), Connect> {
        let expected_revision = self.stored_revision().await?;
        let record = self
            .host
            .client
            .register_client(w::RegisterClientRequest {
                id: self.options.id.clone(),
                name: self.options.name.clone(),
                platform: self.options.platform.clone(),
                capabilities: capabilities::names(),
                capability_specs: capabilities::specs(),
                expected_revision,
                ..Default::default()
            })
            .await?;
        let record = record
            .view()
            .to_owned_message()
            .map_err(|error| Connect::Rejected(error.to_string()))?;
        self.record = Some(record);
        Ok(())
    }

    /// The revision the host stores for this client id, or zero when it has
    /// never seen it.
    async fn stored_revision(&self) -> Result<u64, Connect> {
        let clients = self
            .host
            .client
            .list(w::Empty::default())
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| Connect::Rejected(error.to_string()))?;
        Ok(clients
            .items
            .iter()
            .find(|client| client.id == self.options.id)
            .map_or(0, |client| client.revision))
    }

    /// Starts the renderer and shows the model, if one is configured.
    fn open_avatar(&mut self) -> Result<(), String> {
        let Some(program) = self.options.renderer.as_ref() else {
            tracing::info!("no renderer configured; running headless");
            return Ok(());
        };
        let mut avatar = Avatar::spawn(program)?;
        if let Some(model) = self.options.model.as_ref() {
            avatar.show(&self.options.id, model, None, None)?;
        }
        self.avatar = Some(avatar);
        Ok(())
    }

    /// Reads the current activation for every companion and keeps the one that
    /// names this client. The host is the authority on the generation.
    async fn refresh_activation(&mut self) -> Result<(), Connect> {
        let snapshot = self
            .host
            .host
            .get_snapshot(w::Empty::default())
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| Connect::Rejected(error.to_string()))?;
        let bound = snapshot
            .activations
            .into_iter()
            .find(|activation| activation.client_id.as_deref() == Some(self.options.id.as_str()))
            .map(|activation| Activation {
                companion_id: activation.companion_id,
                revision: activation.revision,
            });
        self.activation = bound.unwrap_or_default();
        Ok(())
    }

    /// Makes this client the active one for `companion_id`.
    pub async fn activate(&mut self, companion_id: &str) -> Result<Activation, Connect> {
        let expected = self
            .host
            .host
            .get_snapshot(w::Empty::default())
            .await?
            .view()
            .activations
            .iter()
            .find(|activation| activation.companion_id == companion_id)
            .map_or(0, |activation| activation.revision);
        let activation = self
            .host
            .client
            .activate(w::ActivationRequest {
                companion_id: companion_id.into(),
                client_id: self.options.id.clone(),
                expected_revision: expected,
                release: false,
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| Connect::Rejected(error.to_string()))?;
        self.activation = Activation {
            companion_id: activation.companion_id,
            revision: activation.revision,
        };
        Ok(self.activation.clone())
    }

    /// The activation this client is bound to.
    pub fn activation(&self) -> &Activation {
        &self.activation
    }

    /// The companion this client should serve: the one already active, or the
    /// host's first companion. A client with no companion has nothing to be the
    /// active device for.
    pub async fn companion(&self) -> Result<String, Connect> {
        let snapshot = self
            .host
            .host
            .get_snapshot(w::Empty::default())
            .await?
            .view()
            .to_owned_message()
            .map_err(|error| Connect::Rejected(error.to_string()))?;
        if let Some(active) = snapshot.active_companion_id {
            return Ok(active);
        }
        snapshot
            .companions
            .first()
            .map(|companion| companion.id.clone())
            .ok_or_else(|| Connect::Unavailable("the host has no companion to serve".into()))
    }

    /// The capabilities the host granted, in declaration order.
    ///
    /// A capability the host did not grant is never dispatched, so this is what
    /// explains a tool the model cannot see: the declaration reached the host,
    /// and the host's policy refused it.
    pub fn granted(&self) -> &[String] {
        self.record
            .as_ref()
            .map_or(&[], |record| record.granted_capabilities.as_slice())
    }

    /// Applies one dispatched operation and returns the result to report.
    ///
    /// The result carries the operation's own generation, so a late answer for
    /// a moved client is discarded by the host rather than by this side.
    pub fn apply(&mut self, operation: &w::DeviceOperation) -> w::DeviceOperationResult {
        let outcome = self.dispatch(operation);
        let (success, output, failure) = match outcome {
            Ok(output) => (true, output.to_string(), None),
            Err(reason) => (
                false,
                String::new(),
                Some(w::Failure {
                    code: "operation_failed".into(),
                    message: reason,
                    ..Default::default()
                }),
            ),
        };
        w::DeviceOperationResult {
            client_id: self.options.id.clone(),
            operation_id: operation.operation_id.clone(),
            success,
            output,
            failure: failure.into(),
            activation_revision: operation.activation_revision,
            ..Default::default()
        }
    }

    fn dispatch(&mut self, operation: &w::DeviceOperation) -> Result<Value, String> {
        let input: Value = if operation.input.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&operation.input)
                .map_err(|error| format!("the operation input is not JSON: {error}"))?
        };
        match operation.capability.as_str() {
            "body.motion" => self.motion(&input),
            "eye.watch" => self.watch(&input),
            other => Err(format!("this client does not implement `{other}`")),
        }
    }

    /// Shows a motion. The renderer owns the clip names; an unknown preset is
    /// the model's mistake and is refused rather than silently ignored.
    fn motion(&mut self, input: &Value) -> Result<Value, String> {
        let motion = input
            .get("motion")
            .and_then(Value::as_str)
            .ok_or("body.motion requires a `motion` name")?;
        if !capabilities::MOTIONS.contains(&motion) {
            return Err(format!("`{motion}` is not a declared motion preset"));
        }
        let expression = input.get("expression").and_then(Value::as_str);
        let Some(avatar) = self.avatar.as_mut() else {
            // No renderer: the operation is accepted and has no visible effect,
            // which is a degraded client rather than a failed operation.
            return Ok(json!({"applied": false, "reason": "no renderer"}));
        };
        let model = self
            .options
            .model
            .as_ref()
            .ok_or("no model is configured for the renderer")?;
        avatar.show(
            &self.options.id,
            model,
            avatar::motion(motion).as_deref(),
            expression,
        )?;
        Ok(json!({"applied": true, "motion": motion}))
    }

    /// Applies or clears a watch rule.
    fn watch(&mut self, input: &Value) -> Result<Value, String> {
        let watch = Watch::parse(input)?;
        let enabled = watch.enabled;
        self.sensor.apply(if enabled { Some(watch) } else { None });
        Ok(json!({"applied": true, "enabled": enabled}))
    }

    /// Samples the OS and reports what it observed.
    pub async fn sample(&mut self) -> Result<(), Connect> {
        let reports = self.sensor.observe(perception::observe());
        for report in reports {
            let event = match report {
                Report::Window(window) => w::PerceptionEvent {
                    event: Some(w::perception_event::Event::ActiveWindow(Box::new(
                        w::ActiveWindowChanged {
                            title: window.title,
                            app_id: window.app_id,
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                },
                Report::Interrupt(window) => w::PerceptionEvent {
                    event: Some(w::perception_event::Event::PerceptionSummary(Box::new(
                        w::PerceptionSummary {
                            summary: format!(
                                "監視対象のウィンドウが前面にあります: {} ({})",
                                window.title, window.app_id
                            ),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                },
                Report::Activity {
                    is_active,
                    idle_seconds,
                } => w::PerceptionEvent {
                    event: Some(w::perception_event::Event::UserActivity(Box::new(
                        w::UserActivityState {
                            is_active,
                            idle_seconds,
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                },
            };
            self.report(event).await?;
        }
        Ok(())
    }

    /// Sends one perception event, bound to the current generation.
    async fn report(&self, event: w::PerceptionEvent) -> Result<(), Connect> {
        self.host
            .client
            .report_perception(w::PerceptionEvent {
                client_id: self.options.id.clone(),
                activation_revision: self.activation.revision,
                timestamp: String::new(),
                ..event
            })
            .await?;
        Ok(())
    }

    /// Reports the result of an operation the host dispatched.
    pub async fn complete(&self, result: w::DeviceOperationResult) -> Result<(), Connect> {
        self.host.client.complete_operation(result).await?;
        Ok(())
    }

    /// The host's microphone stream, bound to the current generation.
    ///
    /// The host answers `unimplemented` when no STT connection is configured,
    /// so a client without one simply has no microphone stream.
    pub async fn microphone_stream(
        &self,
    ) -> Result<
        connectrpc::client::BidiStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            w::AudioInputChunk,
            ene_rpc::__buffa::view::TranscriptionEventView<'static>,
        >,
        Connect,
    > {
        Ok(self.host.voice.stream_audio_input().await?)
    }

    /// The wire chunk for one captured block.
    ///
    /// `is_companion_speaking` is a software flag, not acoustic silence: the
    /// host discards audio captured while it is set, so a client that plays and
    /// records at once does not bind an approval to its own voice.
    pub fn input_chunk(&self, pcm: Vec<u8>) -> w::AudioInputChunk {
        w::AudioInputChunk {
            client_id: self.options.id.clone(),
            pcm_data: pcm,
            sample_rate: audio::WIRE_RATE,
            is_companion_speaking: self.player.as_ref().is_some_and(Player::playing),
            activation_revision: self.activation.revision,
            ..Default::default()
        }
    }

    /// The host's device stream for this client.
    pub async fn device_stream(
        &self,
    ) -> Result<
        connectrpc::client::ServerStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            ene_rpc::__buffa::view::DeviceEventView<'static>,
        >,
        Connect,
    > {
        Ok(self
            .host
            .client
            .device_stream(w::Id {
                id: self.options.id.clone(),
                ..Default::default()
            })
            .await?)
    }

    /// The host's audio output stream, bound to the current generation.
    ///
    /// The host answers `unimplemented` when no TTS connection is configured,
    /// so a client without one simply has no stream.
    pub async fn audio_stream(
        &self,
    ) -> Result<
        connectrpc::client::ServerStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            ene_rpc::__buffa::view::AudioOutputEventView<'static>,
        >,
        Connect,
    > {
        Ok(self
            .host
            .voice
            .subscribe_audio_output(w::SubscribeAudioRequest {
                client_id: self.options.id.clone(),
                activation_revision: self.activation.revision,
                ..Default::default()
            })
            .await?)
    }

    /// Plays one audio event from the host.
    ///
    /// A chunk for a new utterance replaces the old one, because the contract
    /// allows only one audible stream. A flush stops everything at or below its
    /// generation, so a chunk that arrives late is dropped rather than played.
    pub fn play(&mut self, event: w::audio_output_event::Event) {
        let Some(player) = self.player.as_ref() else {
            return;
        };
        match event {
            w::audio_output_event::Event::Chunk(chunk) => {
                if !self.same_utterance(&chunk.utterance_id) {
                    // A new utterance always wins: the old audio is dropped,
                    // never mixed, which is the single-playback invariant.
                    player.begin(&chunk.utterance_id, chunk.generation_id);
                    self.playing_utterance = Some(chunk.utterance_id.clone());
                }
                if let Err(error) =
                    player.push(&chunk.utterance_id, chunk.generation_id, &chunk.pcm_data)
                {
                    tracing::warn!("the audio chunk was not played: {error}");
                }
            }
            w::audio_output_event::Event::Flush(flush) => {
                player.flush(flush.flush_generation_id);
                self.playing_utterance = None;
            }
            w::audio_output_event::Event::Skipped(skipped) => {
                tracing::warn!(
                    utterance = %skipped.utterance_id,
                    sequence = skipped.sequence_no,
                    reason = %skipped.reason,
                    "the host skipped a sentence"
                );
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

    fn same_utterance(&self, utterance: &str) -> bool {
        self.playing_utterance.as_deref() == Some(utterance)
    }

    /// Opens the input device. A machine without a microphone still plays
    /// audio and serves motions.
    pub fn open_microphone(&mut self) {
        match Microphone::open() {
            Ok(microphone) => self.microphone = Some(microphone),
            Err(error) => tracing::warn!("no microphone: {error}"),
        }
    }

    /// The wire chunk for one captured block, or `None` without a microphone.
    pub fn capture(&self) -> Option<w::AudioInputChunk> {
        let microphone = self.microphone.as_ref()?;
        let pcm = microphone.take();
        (!pcm.is_empty()).then(|| self.input_chunk(pcm))
    }

    /// The RMS of the audio being played, for the avatar's lip sync.
    pub fn rms(&self) -> f32 {
        self.player.as_ref().map_or(0., Player::rms)
    }

    /// Sends the current playback amplitude to the renderer.
    ///
    /// Lip sync is a one-way notification, not a request with a reply: it runs
    /// at frame rate and must never block on the renderer's output queue.
    pub fn lip_sync(&mut self) {
        let rms = self.rms();
        if let Some(avatar) = self.avatar.as_mut() {
            avatar.lip_sync(&self.options.id, rms);
        }
    }

    /// Drains the renderer's notifications, so its output queue never fills.
    pub fn drain_notifications(&mut self) -> Vec<Value> {
        self.avatar
            .as_ref()
            .map(avatar::Avatar::notifications)
            .unwrap_or_default()
    }

    /// Stops the renderer.
    pub fn shutdown(&mut self) {
        if let Some(avatar) = self.avatar.take() {
            avatar.shutdown();
        }
    }
}
