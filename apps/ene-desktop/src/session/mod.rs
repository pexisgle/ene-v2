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

mod audio_pipeline;
mod bridge;
mod commands;
mod phase;
mod stream;
mod transport;

use self::{
    audio_pipeline::AudioPipeline, bridge::DeviceBridge, phase::ConnectionPhase,
    transport::SessionTransport,
};
use crate::{
    avatar, capabilities,
    host::{Connect, Discovery, Host},
    perception::Sensor,
    state::{Device, Devices, Link},
    update::{Command, Update},
};
use buffa::view::MessageView;
use ene_rpc as w;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The first wait before a retry, doubled up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
/// How often the client samples the OS for perception.
pub(super) const SAMPLE: Duration = Duration::from_secs(1);
pub(super) const SCREEN_SAMPLE: Duration = Duration::from_secs(20);
/// How long shutdown waits for the last background reports to land.
pub(super) const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How often captured audio goes to the host.
pub(super) const CAPTURE: Duration = Duration::from_millis(100);
/// How often the playback amplitude goes to the renderer (about 60 Hz).
pub(super) const LIP: Duration = Duration::from_millis(16);

fn client_id_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            return Some(PathBuf::from(root).join("ene").join("desktop-client-id"));
        }
    }
    directories::ProjectDirs::from("", "", "ene")
        .map(|dirs| dirs.config_local_dir().join("desktop-client-id"))
}

/// Loads or atomically initializes a stable per-install client ID at `path`.
pub fn load_or_create_client_id(path: &Path) -> String {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() && trimmed.len() <= 128 && !trimmed.contains(char::is_whitespace) {
            return trimmed.to_owned();
        }
    }
    let id = format!("desktop-{}", uuid::Uuid::new_v4());
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
        && let Ok(mut tmp) = tempfile::NamedTempFile::new_in(parent)
    {
        use std::io::Write as _;
        if tmp.write_all(id.as_bytes()).is_ok() {
            ene_platform_os::warn_if_err(tmp.persist(path), "session file persistence");
        }
    }
    id
}

/// Returns the stable per-install client ID used when `ENE_DESKTOP_CLIENT_ID` is unset.
pub fn persistent_client_id() -> String {
    match client_id_path() {
        Some(path) => load_or_create_client_id(&path),
        None => format!("desktop-{}", uuid::Uuid::new_v4()),
    }
}

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
pub(super) struct Binding {
    companion_id: String,
}

/// Why one attempt at serving the host ended.
pub(super) enum Outcome {
    Closed,
    Dropped(String),
    Resync(String),
}

/// What one event asks the serve loop to do.
pub(super) enum Step {
    Continue,
    Restart,
    Stop,
}

pub struct Session {
    transport: SessionTransport,
    audio: AudioPipeline,
    devices: DeviceBridge,
    updates: UnboundedSender<Update>,
    commands: UnboundedReceiver<Command>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("path", &self.transport.path)
            .field("focused", &self.transport.focused)
            .field("subscription", &self.transport.subscription)
            .finish_non_exhaustive()
    }
}

/// One validated `body.motion` request, before anything is sent anywhere.
#[derive(Debug, PartialEq)]
pub(super) struct MotionPlan {
    pub(super) motion: String,
    /// The clip to show. `idle` can fall back to the rest pose.
    pub(super) clip: Option<std::path::PathBuf>,
    /// The face to hold, `neutral` to clear it, or nothing to leave it alone.
    pub(super) expression: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum MotionPlanError {
    #[error("body.motion requires a `motion` name")]
    MissingMotion,
    #[error("`{0}` is not a declared motion preset")]
    UnknownPreset(String),
    #[error("`{0}` is not a declared expression")]
    UnknownExpression(String),
    #[error("the motion asset for `{0}` is not installed, so nothing was shown")]
    MissingMotionAsset(String),
}

impl MotionPlan {
    pub(super) fn parse(input: &serde_json::Value) -> Result<Self, MotionPlanError> {
        let motion = input
            .get("motion")
            .and_then(serde_json::Value::as_str)
            .ok_or(MotionPlanError::MissingMotion)?;
        if !avatar::MOTIONS.iter().any(|(preset, _)| *preset == motion) {
            return Err(MotionPlanError::UnknownPreset(motion.to_owned()));
        }
        let expression = input
            .get("expression")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(name) = &expression
            && !capabilities::EXPRESSIONS.contains(&name.as_str())
        {
            return Err(MotionPlanError::UnknownExpression(name.clone()));
        }
        let clip = avatar::motion(motion);
        if clip.is_none() && motion != "idle" {
            return Err(MotionPlanError::MissingMotionAsset(motion.to_owned()));
        }
        Ok(Self {
            motion: motion.to_owned(),
            clip,
            expression,
        })
    }
}

/// Sends an update to the UI, noting when the UI has already stopped listening.
///
/// The channel closing is expected during shutdown, but dropping the failure
/// silently is how a session that quietly stopped updating goes unnoticed.
pub(crate) fn notify_ui(tx: &UnboundedSender<Update>, update: Update) {
    if tx.send(update).is_err() {
        tracing::debug!("the UI stopped listening; update dropped");
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
        let devices = DeviceBridge::new();
        let transport = SessionTransport::new(
            path,
            options,
            connection,
            devices.workspace.as_ref(),
            &devices.client_plugins,
        );
        Self {
            transport,
            audio: AudioPipeline::new(),
            devices,
            updates,
            commands,
        }
    }

    /// Serves until the window closes or the session is asked to quit.
    pub async fn run(mut self) {
        notify_ui(
            &self.updates,
            Update::EyesEnabled(self.devices.eyes_enabled),
        );
        #[cfg(windows)]
        notify_ui(
            &self.updates,
            Update::ComputerUseEnabled(self.devices.computer_enabled),
        );
        let mut retry = FIRST_RETRY;
        loop {
            let outcome = Box::pin(self.attempt()).await;
            if !self.handle_attempt_outcome(outcome, &mut retry).await {
                break;
            }
        }
        self.devices.shutdown().await;
    }

    async fn handle_attempt_outcome(&mut self, outcome: Outcome, retry: &mut Duration) -> bool {
        match outcome {
            Outcome::Closed => false,
            Outcome::Resync(reason) => {
                // A subscription the host ended is not a dead link: the
                // cursor is dropped so the next attempt resnapshots.
                self.transport.cursor = None;
                tracing::info!("resubscribing: {reason}");
                *retry = FIRST_RETRY;
                tokio::time::sleep(Duration::from_millis(250)).await;
                true
            }
            Outcome::Dropped(reason) => {
                let retry_in = *retry;
                *retry = (*retry * 2).min(MAX_RETRY);
                tracing::warn!("the link dropped: {reason}");
                self.report_phase(ConnectionPhase::Lost { reason, retry_in })
                    .await;
                self.wait_retry_or_control(retry_in).await
            }
        }
    }

    /// Waits out reconnect backoff without ignoring quit or local sensor toggles.
    ///
    /// A local toggle is applied and the wait continues: returning early would
    /// let any command short-circuit the backoff, so a user flipping a switch
    /// during an outage would retry the host as fast as they can click.
    async fn wait_retry_or_control(&mut self, retry_in: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + retry_in;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return true,
                command = self.commands.recv() => match command {
                    Some(Command::Quit) | None => return false,
                    Some(Command::SetEyesEnabled(enabled)) => {
                        self.devices.set_eyes_enabled(enabled, &self.updates);
                    }
                    #[cfg(windows)]
                    Some(Command::SetComputerUseEnabled(enabled)) => {
                        self.devices.set_computer_use_enabled(enabled, &self.updates);
                    }
                    // Anything else is addressed to the live session, which is
                    // not running, so acting on it would do nothing. Keep
                    // waiting: returning here is what let any command cut the
                    // backoff short.
                    Some(command) => {
                        // The two arms above are the local-only commands this
                        // client can act on while disconnected; this one is
                        // everything the domain does not call local-only.
                        debug_assert!(
                            !command.is_local_only(),
                            "a local-only command reached the wait without being applied"
                        );
                    }
                },
            }
        }
    }

    /// One connection attempt: read discovery, register, bind, serve.
    async fn attempt(&mut self) -> Outcome {
        self.transport.set_connection(None);
        let discovery = match Discovery::read(&self.transport.path) {
            Ok(discovery) => discovery,
            Err(Connect::Missing(path)) => {
                self.report_phase(ConnectionPhase::AwaitingDiscovery { path })
                    .await;
                return Outcome::Dropped("接続情報がまだありません".into());
            }
            Err(Connect::Invalid(reason)) => {
                self.report_phase(ConnectionPhase::Unusable {
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
                self.report_phase(ConnectionPhase::Unusable {
                    reason: error.to_string(),
                })
                .await;
                return Outcome::Dropped(error.to_string());
            }
        };
        self.transport.set_connection(Some(host.clone()));
        self.report_phase(ConnectionPhase::Connecting {
            endpoint: discovery.endpoint.clone(),
            attempt: 1,
        })
        .await;

        let _record = match self.transport.register(&host, &self.devices).await {
            Ok(record) => record,
            Err(error) => return self.transport.classify(error),
        };
        self.report_devices().await;

        let snapshot = match host.host.get_snapshot(w::Empty::default()).await {
            Ok(response) => match response.view().to_owned_message() {
                Ok(snapshot) => snapshot,
                Err(error) => return Outcome::Dropped(error.to_string()),
            },
            Err(error) => return self.transport.classify(error),
        };
        let changed = match self.transport.bind(&host, &snapshot).await {
            Ok(changed) => changed,
            Err(outcome) => return outcome,
        };
        if changed {
            self.devices.sensor = Sensor::default();
        }
        self.report_phase(ConnectionPhase::Live {
            version: snapshot.server_version.clone(),
        })
        .await;

        self.open_devices().await;
        Box::pin(self.serve(host)).await
    }

    /// Reports the phase the session just moved into.
    async fn report_phase(&mut self, phase: ConnectionPhase) {
        self.report(phase.to_link()).await;
    }

    /// Reports the link state.
    async fn report(&self, link: Link) {
        notify_ui(&self.updates, Update::Link(Box::new(link)));
    }

    pub(super) async fn open_devices(&mut self) {
        if let Err(state) = self.audio.open_output() {
            self.set_out(state).await;
            return;
        }
        self.report_devices().await;
    }

    pub(super) async fn open_microphone(&mut self) {
        if let Err(state) = self.audio.open_microphone() {
            self.set_in(state).await;
            return;
        }
        self.report_devices().await;
    }

    pub(super) async fn open_avatar(&mut self) {
        match self.devices.open_avatar(&self.transport).await {
            Ok(true) => self.report_devices().await,
            Ok(false) => {}
            Err(state) => self.set_avatar(state).await,
        }
    }

    pub(super) async fn sync_avatars(&mut self) {
        if self.devices.avatar.is_none() {
            return;
        }
        self.devices.sync_avatars(&self.transport).await;
        self.report_devices().await;
    }

    /// Replaces the output device in the footer.
    pub(super) async fn set_out(&mut self, state: Device) {
        notify_ui(
            &self.updates,
            Update::Devices(Box::new(Devices {
                avatar: self.devices.avatar_state(&self.transport),
                output: state,
                input: self.audio.input_state(),
            })),
        );
    }

    pub(super) async fn set_in(&mut self, state: Device) {
        notify_ui(
            &self.updates,
            Update::Devices(Box::new(Devices {
                avatar: self.devices.avatar_state(&self.transport),
                output: self.audio.output_state(),
                input: state,
            })),
        );
    }

    pub(super) async fn set_avatar(&mut self, state: Device) {
        notify_ui(
            &self.updates,
            Update::Devices(Box::new(Devices {
                avatar: state,
                output: self.audio.output_state(),
                input: self.audio.input_state(),
            })),
        );
    }

    /// Sends the current device states.
    pub(super) async fn report_devices(&mut self) {
        let devices = Devices {
            avatar: self.devices.avatar_state(&self.transport),
            output: self.audio.output_state(),
            input: self.audio.input_state(),
        };
        notify_ui(&self.updates, Update::Devices(Box::new(devices)));
    }
}

/// Whether an error means the host has no such feature, rather than a broken link.
pub(super) fn persistent(error: &connectrpc::ConnectError) -> bool {
    matches!(
        error.code,
        connectrpc::ErrorCode::Unimplemented | connectrpc::ErrorCode::FailedPrecondition
    )
}

#[cfg(test)]
mod tests {
    use super::stream::{Claim, admits};
    use super::*;

    /// Every phase must map to the link state the window expects, and only
    /// `Live` may claim the link works. Getting this wrong shows the user a
    /// working connection while the session is still registering.
    #[test]
    fn each_phase_maps_to_the_link_state_the_window_shows() {
        let cases = [
            (
                ConnectionPhase::AwaitingDiscovery {
                    path: "/tmp/discovery.json".into(),
                },
                false,
            ),
            (
                ConnectionPhase::Unusable {
                    reason: "bad token".into(),
                },
                false,
            ),
            (
                ConnectionPhase::Connecting {
                    endpoint: "http://127.0.0.1:1".into(),
                    attempt: 1,
                },
                false,
            ),
            (
                ConnectionPhase::Live {
                    version: "1.0.0".into(),
                },
                true,
            ),
            (
                ConnectionPhase::Lost {
                    reason: "reset".into(),
                    retry_in: Duration::from_secs(1),
                },
                false,
            ),
        ];
        for (phase, live) in cases {
            let link = phase.to_link();
            assert_eq!(
                link.is_live(),
                live,
                "{} mapped to {link:?}",
                match &phase {
                    ConnectionPhase::AwaitingDiscovery { .. } => "AwaitingDiscovery",
                    ConnectionPhase::Unusable { .. } => "Unusable",
                    ConnectionPhase::Connecting { .. } => "Connecting",
                    ConnectionPhase::Live { .. } => "Live",
                    ConnectionPhase::Lost { .. } => "Lost",
                }
            );
        }
    }

    /// A lost link keeps the retry delay it was given, so the window can count
    /// down instead of guessing.
    #[test]
    fn a_lost_phase_carries_its_retry_delay() {
        let phase = ConnectionPhase::Lost {
            reason: "reset".into(),
            retry_in: Duration::from_secs(7),
        };
        match phase.to_link() {
            Link::Lost { retry_in, reason } => {
                assert_eq!(retry_in, Duration::from_secs(7));
                assert_eq!(reason, "reset");
            }
            other => panic!("expected Lost, got {other:?}"),
        }
    }

    fn held() -> Binding {
        Binding {
            companion_id: "companion".into(),
        }
    }

    /// Every case the guard can distinguish: our own identity, our own
    /// subscription, whether we hold the companion, and whether the operation
    /// carries a work session id, each against every operation claim.
    #[test]
    fn only_the_claim_naming_the_current_binding_executes() {
        let our_ids = ["desktop", ""];
        let our_subscriptions = ["sub-1", ""];
        let claim_ids = ["desktop", "other", ""];
        let claim_subscriptions = ["sub-1", "sub-2", ""];
        let held_bindings = [false, true];
        let work_ids = [None, Some(""), Some("work-1")];

        let mut cases = 0usize;
        for our_id in our_ids {
            for our_subscription in our_subscriptions {
                for claim_id in claim_ids {
                    for claim_subscription in claim_subscriptions {
                        for holds in held_bindings {
                            for work_id in work_ids {
                                let binding = holds.then(held);
                                let claim = Claim {
                                    client_id: claim_id,
                                    subscription_id: claim_subscription,
                                    work_id,
                                };
                                // Stated independently of `admits`.
                                let has_work = work_id.is_some_and(|id| !id.is_empty());
                                let expected = claim_id == our_id
                                    && (our_subscription.is_empty()
                                        || claim_subscription == our_subscription)
                                    && (holds || has_work);
                                let allowed =
                                    admits(our_id, our_subscription, binding.as_ref(), &claim);
                                assert_eq!(
                                    allowed.is_ok(),
                                    expected,
                                    "id {our_id:?}/{claim_id:?} subscription {our_subscription:?}/\
{claim_subscription:?} holds {holds:?} work_id {work_id:?}: {allowed:?}"
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2 * 2 * 3 * 3 * 2 * 3, "the domain changed size");
    }

    #[test]
    fn a_refusal_says_which_part_does_not_match() {
        let binding = held();
        let stale = Claim {
            client_id: "desktop",
            subscription_id: "sub-1",
            work_id: None,
        };
        let refused = admits("desktop", "sub-2", Some(&binding), &stale)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("older device stream"), "{refused}");

        let elsewhere = Claim {
            client_id: "desk-1",
            subscription_id: "sub-1",
            work_id: None,
        };
        let refused = admits("desktop", "sub-1", Some(&binding), &elsewhere)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("desk-1"), "{refused}");

        let unbound = admits("desktop", "sub-1", None, &stale)
            .unwrap_err()
            .to_string();
        assert!(unbound.contains("not the active device"), "{unbound}");

        let work_op = Claim {
            client_id: "desktop",
            subscription_id: "sub-1",
            work_id: Some("w1"),
        };
        assert!(admits("desktop", "sub-1", None, &work_op).is_ok());
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
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a declared expression"), "{error}");
        let error = MotionPlan::parse(&serde_json::json!({"motion": "dance"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a declared motion preset"), "{error}");
        let error = MotionPlan::parse(&serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires a `motion`"), "{error}");
    }

    #[test]
    fn b15_persistent_client_id_is_unique_and_stable_across_restarts() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let path_a = dir_a.path().join("desktop-client-id");
        let path_b = dir_b.path().join("desktop-client-id");

        let id_a1 = load_or_create_client_id(&path_a);
        let id_a2 = load_or_create_client_id(&path_a);
        let id_b = load_or_create_client_id(&path_b);
        assert_eq!(id_a1, id_a2);
        assert_ne!(id_a1, id_b);
        assert!(id_a1.starts_with("desktop-"));
    }

    #[tokio::test]
    async fn finished_background_reports_are_reaped_and_the_rest_drained() {
        // The bridge spawns fire-and-forget reports nobody awaits. Without
        // reaping, the set grows for the life of the session; without a drain,
        // shutdown drops reports the host is still waiting for. This covers the
        // two methods themselves; that the frame loop calls `reap_background`
        // each sample tick is a schedule detail pinned by the call site.
        let mut devices = super::bridge::DeviceBridge::new();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..8 {
            let finished = std::sync::Arc::clone(&finished);
            devices.background.lock().unwrap().spawn(async move {
                finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        // Let every task run to completion.
        for _ in 0..50 {
            if finished.load(std::sync::atomic::Ordering::SeqCst) == 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(finished.load(std::sync::atomic::Ordering::SeqCst), 8);

        devices.reap_background();
        assert_eq!(
            devices.background.lock().unwrap().len(),
            0,
            "completed tasks must leave the set"
        );

        // A task that outlives its work must still be drained by shutdown.
        let slow = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&slow);
        devices.background.lock().unwrap().spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        devices.shutdown().await;
        assert_eq!(
            slow.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "shutdown must let a short report finish rather than abort it"
        );
    }
}
