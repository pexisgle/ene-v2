//! Client domain state models organized by responsibility.

use crate::update::CaptureMode;
use ene_rpc as w;
use std::{collections::BTreeMap, time::Duration};

/// The link to the host, in the states the window must tell apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// No discovery file yet: the host has not started.
    Unset { path: String, reason: String },
    /// The discovery file was read; registration and subscription are opening.
    Connecting { endpoint: String, attempt: u32 },
    /// Subscribed and receiving notifications.
    Live { version: String },
    /// The link dropped; the session retries after `retry_in`.
    Lost { reason: String, retry_in: Duration },
    /// Retrying cannot help until the configuration changes.
    Unusable { reason: String },
}

impl Link {
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
    pub avatar: Device,
    pub output: Device,
    pub input: Device,
}

/// A user-visible failure, conflict or refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The action that failed, in the user's words.
    pub context: String,
    pub message: String,
    pub kind: NoticeKind,
}

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
    pub request_key: String,
    pub text: String,
}

/// Text the host did not accept, kept with its key so a retry is not a duplicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsent {
    pub request_key: String,
    pub text: String,
    pub reason: String,
}

/// The selected companion's conversation.
#[derive(Clone, Debug, Default)]
pub struct Chat {
    /// The companion these messages belong to.
    pub companion_id: String,
    /// What the host recorded, oldest first.
    pub messages: Vec<w::Message>,
    pub loading: bool,
    pub error: Option<String>,
    pub sending: Vec<Sending>,
    pub unsent: Vec<Unsent>,
    /// The most recent turn for this companion.
    pub turn: Option<w::TurnEvent>,
    /// A stop the host accepted but has not finished.
    pub stop_requested: bool,
}

/// Live speech-to-text recognition subtitle shown while listening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTranscription {
    pub text: String,
    pub is_final: bool,
    pub is_speaking: bool,
}

/// Local capture diagnostics shown in settings probes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Capture {
    pub level: f32,
    pub open: bool,
    pub sent: usize,
}

/// A readback line the speech tab keeps after its request finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechReport {
    pub text: String,
    pub failed: bool,
}

/// What the window should refetch because a notification invalidated it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub history: bool,
    pub approvals: bool,
    pub mind: bool,
}

/// Connection lifecycle, provider catalog, model cache, and device status.
#[derive(Debug)]
pub struct ConnectionViewState {
    pub link: Link,
    pub link_epoch: u64,
    pub providers: Option<Vec<w::LlmProvider>>,
    pub models: BTreeMap<String, Vec<w::Model>>,
    pub checks: BTreeMap<String, w::ConnectionCheck>,
    pub devices: Devices,
    pub plugins: Vec<w::PluginInfo>,
}

impl ConnectionViewState {
    pub fn new(discovery: String) -> Self {
        Self {
            link: Link::Unset {
                path: discovery,
                reason: String::new(),
            },
            link_epoch: 1,
            providers: None,
            models: BTreeMap::new(),
            checks: BTreeMap::new(),
            devices: Devices::default(),
            plugins: Vec::new(),
        }
    }
}

/// Companion selection, active conversation, mind views, and starter templates.
#[derive(Debug, Default)]
pub struct CompanionState {
    pub selected: Option<String>,
    pub chat: Chat,
    pub streaming: BTreeMap<(String, String), String>,
    pub minds: BTreeMap<String, w::MindView>,
    pub work_active: bool,
    pub templates: Vec<w::CompanionTemplate>,
    pub exported_template: Option<(String, String)>,
    pub preview_reply: Option<Result<String, String>>,
}

/// Audio levels, mute flags, capture mode, and live speech recognition state.
#[derive(Debug)]
pub struct AudioState {
    pub output_rms: f32,
    pub input_rms: f32,
    pub companion_speaking: bool,
    pub mic_muted: bool,
    pub capture_mode: CaptureMode,
    pub ptt_active: bool,
    pub output_muted: bool,
    pub transcription: Option<LiveTranscription>,
    pub capture: Capture,
    pub speech_report: Option<SpeechReport>,
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
    pub eyes_enabled: bool,
    #[cfg(windows)]
    pub computer_use_enabled: bool,
    pub eyes_status: Option<String>,
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
    pub last_resolved_id: Option<String>,
    pub last_rule_warning: Option<String>,
}

/// Everything the window draws, organized by domain sub-state.
#[derive(Debug)]
pub struct Model {
    pub snapshot: Option<w::Snapshot>,
    pub notices: Vec<Notice>,
    pub connections: ConnectionViewState,
    pub companions: CompanionState,
    pub audio: AudioState,
    pub perception: PerceptionState,
    pub approval: ApprovalState,
}

impl Model {
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
    pub fn snapshot(&self) -> Option<&w::Snapshot> {
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
    pub fn companion(&self) -> Option<&w::Companion> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .companions
            .iter()
            .find(|companion| companion.id == id)
    }

    /// The activation the host holds for the shown companion.
    pub fn activation(&self) -> Option<&w::Activation> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .activations
            .iter()
            .find(|activation| activation.companion_id == id)
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
                .map(|activation| activation.companion_id.clone())
                .collect()
        })
    }

    /// Whether `companion_id` is currently resident on `client_id`.
    pub fn is_resident(&self, companion_id: &str, client_id: &str) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.activations.iter().any(|activation| {
                activation.companion_id == companion_id
                    && activation.client_id.as_deref() == Some(client_id)
            })
        })
    }

    /// The settings the window shows.
    pub fn settings(&self) -> Option<&w::Settings> {
        self.snapshot.as_ref()?.settings.as_option()
    }

    pub fn policy_texts(&self) -> &[w::PolicyText] {
        self.settings()
            .map_or(&[], |settings| settings.policy_texts.as_slice())
    }

    /// The pending approvals, oldest first.
    pub fn approvals(&self) -> &[w::ApprovalRequest] {
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
