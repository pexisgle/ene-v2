//! Client domain state models organized by responsibility.

mod chat;
pub use chat::{Chat, Sending, Unsent};

use crate::domain::{
    Activation, ApprovalRequest, Companion, CompanionTemplate, Connection, LlmProvider, MindView,
    ModelAssignment, ModelInfo, PluginInfo, PolicyText, Settings, Snapshot,
};
use crate::update::{CaptureMode, ReplyOrder, ReplyTarget};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

/// The link to the host, in the states the window must tell apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// No discovery file yet: the host has not started.
    Unset {
        /// Path the discovery file was looked for at.
        path: String,
        /// Why the file was not usable.
        reason: String,
    },
    /// The discovery file was read; registration and subscription are opening.
    Connecting {
        /// Endpoint taken from the discovery file.
        endpoint: String,
        /// Which connection attempt this is.
        attempt: u32,
    },
    /// Subscribed and receiving notifications.
    Live {
        /// Host version reported at registration.
        version: String,
    },
    /// The link dropped; the session retries after `retry_in`.
    Lost {
        /// What went wrong.
        reason: String,
        /// How long the session waits before retrying.
        retry_in: Duration,
    },
    /// Retrying cannot help until the configuration changes.
    Unusable {
        /// What the operator has to change.
        reason: String,
    },
}

impl Link {
    /// Whether the link is subscribed and receiving notifications.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// One local device the client owns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Device {
    /// Not opened: no renderer configured, or the output was left off.
    #[default]
    Off,
    /// Open, with the detail worth showing (rate, model path, device name).
    Ready(String),
    /// Wanted but unavailable, with the reason.
    Unavailable(String),
}

/// The client's own devices, as the footer shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Devices {
    /// The avatar renderer.
    pub avatar: Device,
    /// The audio output.
    pub output: Device,
    /// The microphone.
    pub input: Device,
}

/// A user-visible failure, conflict or refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The action that failed, in the user's words.
    pub context: String,
    /// What happened, written for the operator.
    pub message: String,
    /// Which kind of problem this is, so the window can style it.
    pub kind: NoticeKind,
}

/// Which kind of problem a notice reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// The request did not happen.
    Failed,
    /// The host's revision moved; the window refetched and the operator retries.
    Conflict,
    /// The credential or configuration was refused; retrying will not help.
    Denied,
}

/// Live speech-to-text recognition subtitle shown while listening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTranscription {
    /// The recognised text so far.
    pub text: String,
    /// Whether the provider considers this the final transcript.
    pub is_final: bool,
    /// Whether the person is speaking right now.
    pub is_speaking: bool,
}

/// Local capture diagnostics shown in settings probes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Capture {
    /// Current input level, normalised.
    pub level: f32,
    /// Whether the capture device is open.
    pub open: bool,
    /// How many samples have been sent.
    pub sent: usize,
}

/// A readback line the speech tab keeps after its request finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechReport {
    /// The text that was read back.
    pub text: String,
    /// Whether the readback failed.
    pub failed: bool,
}

/// What the window should refetch because a notification invalidated it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// The conversation history needs refetching.
    pub history: bool,
    /// The host state needs a fresh snapshot.
    pub snapshot: bool,
    /// The mind view needs refetching.
    pub mind: bool,
    /// The plugin registry view needs refetching.
    pub plugins: bool,
}

/// Connection lifecycle, provider catalog, model cache, and device status.
#[derive(Debug)]
pub struct ConnectionViewState {
    /// Where the link to the host stands.
    pub link: Link,
    /// Bumped on every new link, so stale replies can be dropped.
    pub link_epoch: u64,
    pub(crate) replies: HashMap<ReplyTarget, ReplyOrder>,
    pub(crate) order: crate::notifications::HostOrder,
    /// The provider catalog, once fetched.
    pub providers: Option<Vec<LlmProvider>>,
    /// Models per connection id, once fetched.
    pub models: BTreeMap<String, Vec<ModelInfo>>,
    /// The client's own devices.
    pub devices: Devices,
    /// Installed plugins, once fetched.
    pub plugins: Vec<PluginInfo>,
}

impl ConnectionViewState {
    /// Starts in the unset state, waiting for the discovery file at `discovery`.
    pub fn new(discovery: String) -> Self {
        Self {
            link: Link::Unset {
                path: discovery,
                reason: String::new(),
            },
            link_epoch: 1,
            replies: HashMap::new(),
            order: Default::default(),
            providers: None,
            models: BTreeMap::new(),
            devices: Devices::default(),
            plugins: Vec::new(),
        }
    }
}

/// Why a companion preview reply failed, in words the person can read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PreviewError(pub String);

/// Companion selection, active conversation, mind views, and starter templates.
#[derive(Debug, Default)]
pub struct CompanionState {
    /// The companion the operator selected, when they picked one.
    pub selected: Option<String>,
    /// The selected companion's conversation.
    pub chat: Chat,
    /// Reply text still arriving, keyed by companion and stream id.
    pub streaming: BTreeMap<(String, String), String>,
    /// Mind views per companion id, once fetched.
    pub minds: BTreeMap<String, MindView>,
    /// Starter templates the host offers.
    pub templates: Vec<CompanionTemplate>,
    /// The last export, as a file name and its JSON.
    pub exported_template: Option<(String, String)>,
    /// The last preview reply, or why it failed.
    pub preview_reply: Option<Result<String, PreviewError>>,
}

/// Audio levels, mute flags, capture mode, and live speech recognition state.
#[derive(Clone, Debug)]
pub struct AudioState {
    /// Output level, normalised.
    pub output_rms: f32,
    /// Input level, normalised.
    pub input_rms: f32,
    /// Whether the companion is talking right now.
    pub companion_speaking: bool,
    /// Whether the microphone is muted.
    pub mic_muted: bool,
    /// How the microphone decides when to capture.
    pub capture_mode: CaptureMode,
    /// Whether push-to-talk is holding the microphone open.
    pub ptt_active: bool,
    /// Whether the audio output is muted.
    pub output_muted: bool,
    /// The recognition subtitle, while listening.
    pub transcription: Option<LiveTranscription>,
    /// Capture diagnostics from the last probe.
    pub capture: Capture,
    /// The last speech readback result.
    pub speech_report: Option<SpeechReport>,
    /// Phrases recognised so far this session.
    pub recognized: Vec<String>,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            output_rms: 0.0,
            input_rms: 0.0,
            companion_speaking: false,
            mic_muted: false,
            capture_mode: CaptureMode::Vad,
            ptt_active: false,
            output_muted: false,
            transcription: None,
            capture: Capture::default(),
            speech_report: None,
            recognized: Vec::new(),
        }
    }
}

/// Perception (eyes / computer use) and avatar visual state.
#[derive(Debug)]
pub struct PerceptionState {
    /// Local consent and the availability of its observer.
    pub eyes: EyesState,
    /// Whether the host may drive the desktop.
    #[cfg(windows)]
    pub computer_use_enabled: bool,
    /// The perception status line, when the host reported one.
    pub eyes_status: Option<String>,
    /// Avatar scale as a percentage.
    pub avatar_scale: f32,
}

impl Default for PerceptionState {
    fn default() -> Self {
        Self {
            eyes: EyesState::Off,
            #[cfg(windows)]
            computer_use_enabled: false,
            eyes_status: None,
            avatar_scale: 100.0,
        }
    }
}

/// The desktop's local opt-in and native observation availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EyesState {
    /// The user has not enabled observation on this device.
    Off,
    /// The user opted in but the native observer could not start.
    Unavailable,
    /// A native observer is available to this device.
    Available,
}

impl EyesState {
    /// Whether the local preference permits observation.
    pub fn enabled(self) -> bool {
        self != Self::Off
    }
}

/// Everything the window draws, organized by domain sub-state.
#[derive(Debug)]
pub struct Model {
    /// The last authoritative host snapshot.
    pub snapshot: Option<Snapshot>,
    /// Recent user-visible failures, oldest first.
    pub notices: Vec<Arc<Notice>>,
    /// Link, provider, model and device state.
    pub connections: ConnectionViewState,
    /// Companion selection, chat and templates.
    pub companions: CompanionState,
    /// Audio and recognition state.
    pub audio: AudioState,
    /// Perception and avatar state.
    pub perception: PerceptionState,
}

impl Model {
    /// Starts in the unset state, waiting for the discovery file at `discovery`.
    pub fn new(discovery: String) -> Self {
        Self {
            snapshot: None,
            notices: Vec::new(),
            connections: ConnectionViewState::new(discovery),
            companions: CompanionState::default(),
            audio: AudioState::default(),
            perception: PerceptionState::default(),
        }
    }

    /// The snapshot, when one has arrived.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// Whether any work session is active in the current host view.
    pub fn has_active_work(&self) -> bool {
        self.snapshot()
            .is_some_and(|snapshot| !snapshot.active_work.is_empty())
    }

    /// The companion the window is showing.
    pub fn companion_id(&self) -> Option<&str> {
        self.companions
            .selected
            .as_deref()
            .or_else(|| self.snapshot.as_ref()?.active_companion_id.as_deref())
            .or_else(|| {
                self.snapshot
                    .as_ref()?
                    .companions
                    .first()
                    .map(|item| item.id.as_str())
            })
    }

    /// The companion record for the shown id.
    pub fn companion(&self) -> Option<&Companion> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .companions
            .iter()
            .find(|companion| companion.id == id)
    }

    /// The selected companion's dialogue model, including an unavailable saved assignment.
    pub fn dialogue_assignment(&self) -> Option<&ModelAssignment> {
        let assignment = &self.companion()?.assignments.as_ref()?.companion;
        assignment.is_set().then_some(assignment)
    }

    /// Effective receiver displayed before sending data to a saved dialogue model.
    pub fn dialogue_receiver(&self, connection: &Connection) -> Option<String> {
        let endpoint = connection.llm_endpoint()?;
        if endpoint.adapter == crate::GOOGLE_AGENT_PLATFORM {
            return ene_domain::settings::agent_platform_publisher_endpoint(endpoint).ok();
        }
        if !endpoint.endpoint.trim().is_empty() {
            return Some(endpoint.endpoint.trim().to_owned());
        }
        self.connections
            .providers
            .as_ref()?
            .iter()
            .find(|provider| provider.adapter == endpoint.adapter)
            .map(|provider| provider.endpoint.clone())
            .filter(|endpoint| !endpoint.is_empty())
    }

    /// The activation the host holds for the shown companion.
    pub fn activation(&self) -> Option<&Activation> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .activations
            .iter()
            .find(|activation| activation.companion_id.as_str() == id)
    }

    /// Whether this client is the shown companion's active device.
    pub fn is_active_client(&self, client_id: &str) -> bool {
        self.activation()
            .and_then(|activation| activation.client_id.as_deref())
            == Some(client_id)
    }

    /// All companion IDs currently resident on `client_id`.
    pub fn resident_companions(&self, client_id: &str) -> Vec<String> {
        self.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .activations
                .iter()
                .filter(|activation| activation.client_id.as_deref() == Some(client_id))
                .map(|activation| activation.companion_id.to_string())
                .collect()
        })
    }

    /// Whether `companion_id` is currently resident on `client_id`.
    pub fn is_resident(&self, companion_id: &str, client_id: &str) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.activations.iter().any(|activation| {
                activation.companion_id.as_str() == companion_id
                    && activation.client_id.as_deref() == Some(client_id)
            })
        })
    }

    /// The settings the window shows.
    pub fn settings(&self) -> Option<&Settings> {
        self.snapshot.as_ref()?.settings.as_ref()
    }

    /// The free-text policies from the current snapshot, empty until one arrives.
    pub fn policy_texts(&self) -> &[PolicyText] {
        self.settings()
            .map_or(&[], |settings| settings.policy_texts.as_slice())
    }

    /// The pending approvals, oldest first.
    pub fn approvals(&self) -> &[ApprovalRequest] {
        self.snapshot
            .as_ref()
            .map_or(&[], |snapshot| snapshot.pending_approvals.as_slice())
    }

    /// Adds a user-visible failure, keeping only the recent ones.
    pub fn notice(
        &mut self,
        kind: NoticeKind,
        context: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push_notice(Notice {
            context: context.into(),
            message: message.into(),
            kind,
        });
    }

    pub(crate) fn push_notice(&mut self, notice: Notice) {
        self.notices.push(Arc::new(notice));
        if self.notices.len() > 8 {
            self.notices.remove(0);
        }
    }

    /// Dismisses the notice shown to the operator, even if the list has moved.
    pub fn dismiss_notice(&mut self, notice: &Arc<Notice>) -> bool {
        let Some(index) = self
            .notices
            .iter()
            .position(|shown| Arc::ptr_eq(shown, notice))
        else {
            return false;
        };
        self.notices.remove(index);
        true
    }
}
