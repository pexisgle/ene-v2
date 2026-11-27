//! Client domain state models organized by responsibility.

use crate::domain::{
    Activation, ApprovalRequest, Companion, CompanionTemplate, ConnectionCheck, LlmProvider,
    Message, MindView, ModelInfo, PluginInfo, PolicyText, Settings, Snapshot, TurnEvent,
};
use crate::update::CaptureMode;
use ene_domain::CompanionId;
use std::{collections::BTreeMap, time::Duration};

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

/// Text the operator sent whose acceptance has not arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sending {
    /// Client-side key that makes a retry idempotent.
    pub request_key: String,
    /// The text that was sent.
    pub text: String,
}

/// Text the host did not accept, kept with its key so a retry is not a duplicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsent {
    /// Client-side key that makes the retry idempotent.
    pub request_key: String,
    /// The text that was not accepted.
    pub text: String,
    /// Why the host refused it.
    pub reason: String,
}

/// The selected companion's conversation.
#[derive(Clone, Debug, Default)]
pub struct Chat {
    /// The companion these messages belong to; `None` until one is selected.
    pub companion_id: Option<CompanionId>,
    /// What the host recorded, oldest first.
    pub messages: Vec<Message>,
    /// Whether the history is being fetched.
    pub loading: bool,
    /// Why the last fetch failed, when it did.
    pub error: Option<String>,
    /// Text sent but not yet acknowledged.
    pub sending: Vec<Sending>,
    /// Text the host refused, kept for a retry.
    pub unsent: Vec<Unsent>,
    /// The most recent turn for this companion.
    pub turn: Option<TurnEvent>,
    /// A stop the host accepted but has not finished.
    pub stop_requested: bool,
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
    /// The pending approvals need refetching.
    pub approvals: bool,
    /// The mind view needs refetching.
    pub mind: bool,
}

/// Connection lifecycle, provider catalog, model cache, and device status.
#[derive(Debug)]
pub struct ConnectionViewState {
    /// Where the link to the host stands.
    pub link: Link,
    /// Bumped on every new link, so stale replies can be dropped.
    pub link_epoch: u64,
    /// Highest `Update::Scoped` sequence applied in `link_epoch`.
    ///
    /// RPC replies race: two requests start in order, and the earlier one can
    /// arrive second. Its `seq` is lower, so applying it would overwrite newer
    /// state with an older answer.
    pub(crate) applied_seq: u64,
    /// The provider catalog, once fetched.
    pub providers: Option<Vec<LlmProvider>>,
    /// Models per connection id, once fetched.
    pub models: BTreeMap<String, Vec<ModelInfo>>,
    /// Last probe result per connection id.
    pub checks: BTreeMap<String, ConnectionCheck>,
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
            applied_seq: 0,
            providers: None,
            models: BTreeMap::new(),
            checks: BTreeMap::new(),
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
    /// Whether a work session is running.
    pub work_active: bool,
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
    /// Whether screen perception is on.
    pub eyes_enabled: bool,
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
            eyes_enabled: false,
            #[cfg(windows)]
            computer_use_enabled: false,
            eyes_status: None,
            avatar_scale: 100.0,
        }
    }
}

/// Approval resolution and policy rule tracking state.
#[derive(Debug, Default)]
pub struct ApprovalState {
    /// The approval most recently answered.
    pub last_resolved_id: Option<String>,
    /// A warning about a saved rule, when one needs saying.
    pub last_rule_warning: Option<String>,
}

/// Everything the window draws, organized by domain sub-state.
#[derive(Debug)]
pub struct Model {
    /// The last authoritative host snapshot.
    pub snapshot: Option<Snapshot>,
    /// Recent user-visible failures, oldest first.
    pub notices: Vec<Notice>,
    /// Link, provider, model and device state.
    pub connections: ConnectionViewState,
    /// Companion selection, chat and templates.
    pub companions: CompanionState,
    /// Audio and recognition state.
    pub audio: AudioState,
    /// Perception and avatar state.
    pub perception: PerceptionState,
    /// Approval and policy tracking.
    pub approval: ApprovalState,
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
            approval: ApprovalState::default(),
        }
    }

    /// The snapshot, when one has arrived.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
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
        self.notices.push(Notice {
            context: context.into(),
            message: message.into(),
            kind,
        });
        if self.notices.len() > 8 {
            self.notices.remove(0);
        }
    }
}
