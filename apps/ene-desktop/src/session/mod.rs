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

mod bridge;
mod commands;
mod stream;

use crate::{
    audio::{self, Microphone, Player},
    avatar::{self, Avatar},
    capabilities,
    host::{Connect, Discovery, Host},
    perception::{self, Sensor},
    state::Link,
    update::{Command, Update},
    work_tools::Workspace,
};
use buffa::view::MessageView;
use ene_rpc as w;
use std::{
    collections::{BTreeMap, HashMap},
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
            let _ = tmp.persist(path);
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
    pub(super) path: PathBuf,
    pub(super) options: Options,
    pub(super) connection: Connection,
    pub(super) updates: UnboundedSender<Update>,
    pub(super) commands: UnboundedReceiver<Command>,
    /// Companions currently resident (activated) on this client, keyed by companion ID.
    pub(super) bindings: BTreeMap<String, Binding>,
    /// Companion currently focused by the pod / interactive controls.
    pub(super) focused: Option<String>,
    /// Latest companion records from the host, used to resolve body/model per resident companion.
    pub(super) companions: BTreeMap<String, w::Companion>,
    /// Companion currently owning active TTS playback for lip-sync attribution.
    pub(super) speaking_companion: Option<String>,
    /// Whether the next attempt may move the activation. It is set on the first
    /// connect and when the operator picks a companion, so a reconnect never
    /// steals the companion back from another device.
    pub(super) may_activate: bool,
    /// The surface of the current device stream, from `DeviceReady`.
    pub(super) subscription: String,
    /// The last notification cursor, for a resuming subscription.
    pub(super) cursor: Option<w::Cursor>,
    pub(super) avatar: Option<Avatar>,
    pub(super) player: Option<Player>,
    pub(super) microphone: Option<Microphone>,
    pub(super) sensor: Sensor,
    pub(super) eyes_enabled: bool,
    #[cfg(windows)]
    pub(super) computer_enabled: bool,
    #[cfg(windows)]
    pub(super) computer_frame: Option<crate::computer::Frame>,
    /// The companion the operator asked to serve while a request was in flight.
    pub(super) pending: Option<String>,
    /// Capabilities this machine can honour, declared at registration.
    pub(super) specs: Vec<w::ClientCapabilitySpec>,
    pub(super) workspace: Option<Arc<Workspace>>,
    /// Verified client-side Wasm plugins (`F11`).
    pub(super) client_plugins: Arc<crate::plugins::ClientPlugins>,
    /// Long-lived workspace operations so settings updates/resyncs never abort them.
    pub(super) work: tokio::task::JoinSet<w::DeviceOperationResult>,
    /// Last amplitude sent per character, so silence sends nothing new.
    pub(super) last_lip: HashMap<String, f32>,
    /// Per-character debounce timestamp for avatar click/interaction perception (`F21`).
    pub(super) last_avatar_interaction: HashMap<String, std::time::Instant>,
    /// Software mute for the microphone.
    pub(super) mic_muted: bool,
    /// Active capture mode (VAD or Push-to-Talk) (`F07`).
    pub(super) capture_mode: audio::CaptureMode,
    pub(super) ptt_active: bool,
    /// Software mute for speaker output.
    pub(super) output_muted: bool,
    /// Current avatar display scale percentage.
    pub(super) avatar_scale: f32,
    /// Last audio state forwarded to the UI, to avoid spamming idle frames.
    pub(super) last_ui_audio: (f32, f32, bool),
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("path", &self.path)
            .field("focused", &self.focused)
            .field("subscription", &self.subscription)
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
        let client_plugins = Arc::new(crate::plugins::ClientPlugins::probe_default());
        specs.extend(client_plugins.specs().iter().cloned());
        Self {
            path,
            options,
            connection,
            updates,
            commands,
            bindings: BTreeMap::new(),
            focused: None,
            companions: BTreeMap::new(),
            speaking_companion: None,
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
            last_lip: HashMap::new(),
            last_avatar_interaction: HashMap::new(),
            mic_muted: false,
            capture_mode: audio::CaptureMode::Vad,
            ptt_active: false,
            output_muted: false,
            avatar_scale: 100.,
            last_ui_audio: (0., 0., false),
            pending: None,
            specs,
            workspace,
            client_plugins,
            work: tokio::task::JoinSet::new(),
        }
    }

    pub(super) fn focused_companion_id(&self) -> Option<&str> {
        self.focused
            .as_deref()
            .filter(|id| self.bindings.contains_key(*id))
            .or_else(|| self.bindings.keys().next().map(String::as_str))
    }

    pub(super) fn focused_binding(&self) -> Option<&Binding> {
        self.focused_companion_id()
            .and_then(|id| self.bindings.get(id))
    }

    pub(super) fn body_binding_for(&self, companion_id: &str) -> w::BodyBinding {
        self.companions
            .get(companion_id)
            .and_then(|companion| companion.body.as_option())
            .and_then(|doc| serde_json::from_str::<w::BodyBinding>(&doc.json).ok())
            .unwrap_or_default()
    }

    pub(super) fn model_for(&self, companion_id: &str) -> Option<PathBuf> {
        let body = self.body_binding_for(companion_id);
        avatar::resolve_model_path(body.model.as_deref(), self.options.model.as_deref())
    }

    fn rebuild_specs(&mut self) {
        let mut specs = capabilities::Declaration::probe(
            self.options.renderer.as_deref(),
            self.options.model.as_deref(),
        )
        .specs();
        if let Some(workspace) = &self.workspace {
            specs.extend(workspace.specs());
        }
        specs.extend(self.client_plugins.specs().iter().cloned());
        self.specs = specs;
    }

    pub(super) async fn sync_capabilities(&mut self, host: &Host) {
        self.rebuild_specs();
        if let Err(error) = self.register(host).await {
            tracing::warn!("failed to sync client capabilities in place: {error}");
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
            let outcome = Box::pin(self.attempt()).await;
            if !self.handle_attempt_outcome(outcome, &mut retry).await {
                break;
            }
        }
        self.shutdown();
    }

    async fn handle_attempt_outcome(&mut self, outcome: Outcome, retry: &mut Duration) -> bool {
        match outcome {
            Outcome::Closed => false,
            Outcome::Resync(reason) => {
                // A subscription the host ended is not a dead link: the
                // cursor is dropped so the next attempt resnapshots.
                self.cursor = None;
                tracing::info!("resubscribing: {reason}");
                *retry = FIRST_RETRY;
                tokio::time::sleep(Duration::from_millis(250)).await;
                true
            }
            Outcome::Dropped(reason) => {
                let retry_in = *retry;
                *retry = (*retry * 2).min(MAX_RETRY);
                tracing::warn!("the link dropped: {reason}");
                self.report(Link::Lost { reason, retry_in }).await;
                self.wait_retry_or_control(retry_in).await
            }
        }
    }

    /// Waits out reconnect backoff without ignoring quit or local sensor toggles.
    async fn wait_retry_or_control(&mut self, retry_in: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(retry_in) => true,
            command = self.commands.recv() => match command {
                Some(Command::SetEyesEnabled(enabled)) => {
                    self.set_eyes_enabled(enabled);
                    true
                }
                #[cfg(windows)]
                Some(Command::SetComputerUseEnabled(enabled)) => {
                    self.set_computer_use_enabled(enabled);
                    true
                }
                Some(Command::Quit) | None => false,
                _ => true,
            },
        }
    }

    /// One connection attempt: read discovery, register, bind, serve.
    async fn attempt(&mut self) -> Outcome {
        *self
            .connection
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
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
        *self
            .connection
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(host.clone());
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
        let previous = self.bindings.clone();
        if let Err(outcome) = self.bind(&host, &snapshot).await {
            return outcome;
        }
        if previous != self.bindings {
            self.sensor = Sensor::default();
        }
        self.report(Link::Live {
            version: snapshot.server_version.clone(),
        })
        .await;

        self.open_devices().await;
        Box::pin(self.serve(host)).await
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

    /// Binds this client to its resident companions from `snapshot` without
    /// stealing companions already held by other devices.
    async fn bind(&mut self, host: &Host, snapshot: &w::Snapshot) -> Result<(), Outcome> {
        self.companions = snapshot
            .companions
            .iter()
            .map(|companion| (companion.id.clone(), companion.clone()))
            .collect();
        self.bindings = snapshot
            .activations
            .iter()
            .filter(|activation| activation.client_id.as_deref() == Some(self.options.id.as_str()))
            .map(|activation| {
                (
                    activation.companion_id.clone(),
                    Binding {
                        companion_id: activation.companion_id.clone(),
                    },
                )
            })
            .collect();

        if !self.bindings.is_empty() {
            // Re-attaching to an existing host activation must consume initial
            // activation intent so a future move away never auto-steals back (B16).
            self.may_activate = false;
            return self.rebind_existing(host, snapshot).await;
        }

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
            self.bindings.clear();
            self.focused = None;
            return Ok(());
        };
        if !self.may_activate {
            // Another device holds this companion. Serving nothing is the
            // honest state: no audio, no perception, no device results.
            self.bindings.clear();
            self.focused = None;
            self.pending = None;
            tracing::info!("another client is the active device for {wanted}");
            return Ok(());
        }
        match self.activate_one(host, &wanted).await {
            Ok(binding) => {
                self.bindings.insert(wanted.clone(), binding);
                self.focused = Some(wanted);
                self.may_activate = false;
                self.pending = None;
                Ok(())
            }
            Err(error) => Err(self.classify(error)),
        }
    }

    async fn rebind_existing(
        &mut self,
        host: &Host,
        snapshot: &w::Snapshot,
    ) -> Result<(), Outcome> {
        if let Some(target) = self.pending.take() {
            if !self.bindings.contains_key(&target) && self.companions.contains_key(&target) {
                match self.activate_one(host, &target).await {
                    Ok(binding) => {
                        self.bindings.insert(target.clone(), binding);
                    }
                    Err(error) => return Err(self.classify(error)),
                }
            }
            if self.bindings.contains_key(&target) {
                self.focused = Some(target);
            }
        }
        if self
            .focused
            .as_deref()
            .is_none_or(|id| !self.bindings.contains_key(id))
        {
            self.focused = snapshot
                .active_companion_id
                .as_deref()
                .filter(|id| self.bindings.contains_key(*id))
                .map(str::to_owned)
                .or_else(|| self.bindings.keys().next().cloned());
        }
        Ok(())
    }

    /// Activates `companion_id` onto this client without releasing other
    /// companions already resident here.
    pub(super) async fn activate_one(
        &self,
        host: &Host,
        companion_id: &str,
    ) -> Result<Binding, connectrpc::ConnectError> {
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
            "the client is an active device"
        );
        Ok(Binding {
            companion_id: activation.companion_id,
        })
    }

    /// Releases `companion_id` from this client.
    pub(super) async fn release_one(
        &self,
        host: &Host,
        companion_id: &str,
    ) -> Result<(), connectrpc::ConnectError> {
        host.client
            .activate(w::ActivationRequest {
                companion_id: companion_id.to_owned(),
                client_id: String::new(),
                release: true,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Reports the link state.
    async fn report(&self, link: Link) {
        let _ = self.updates.send(Update::Link(Box::new(link)));
    }

    /// Turns an RPC error into what the window must show.
    pub(super) fn classify(&self, error: connectrpc::ConnectError) -> Outcome {
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
}
