//! The two messages across the one boundary the client needs: the session
//! reports to the window, and the window asks the session to act.

use crate::state::{Devices, Link};
use ene_rpc as w;

/// Microphone capture mode (`F07`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    /// Continuous voice-activity detection.
    #[default]
    Vad,
    /// Push-to-Talk: captures only while held, commits on release, discards on cancel.
    Ptt,
}

/// One report from the background session or one finished request.
#[derive(Debug)]
pub enum Update {
    /// The link state changed.
    Link(Box<Link>),
    /// One host notification, verbatim, for the window to merge.
    Notification(Box<w::Notification>),
    /// The host ended the subscription and the session is resubscribing.
    Resync,
    /// A local device changed state.
    Devices(Box<Devices>),
    /// An action the operator asked for did not happen.
    Failed {
        context: &'static str,
        message: String,
    },
    /// A revision the window used was stale; the window refetches the authority.
    Conflict {
        context: &'static str,
        message: String,
    },
    /// The host refused the owner credential or the endpoint.
    Denied {
        context: &'static str,
        message: String,
    },
    /// A message was accepted into the conversation.
    Accepted {
        request_key: String,
        request_id: String,
    },
    /// A message was not accepted; the text stays with the operator.
    SendFailed {
        request_key: String,
        text: String,
        reason: String,
    },
    /// The host accepted a stop request.
    StopRequested {
        companion_id: String,
    },
    /// The host synthesized and dispatched a probe utterance.
    Spoke,
    /// The probe could not be synthesized or dispatched.
    SpokeFailed {
        message: String,
    },
    /// The conversation the window asked for.
    History {
        companion_id: String,
        messages: Vec<w::Message>,
    },
    HistoryFailed {
        companion_id: String,
        message: String,
    },
    /// The provider catalog.
    Providers(Vec<w::LlmProvider>),
    SetupFinished {
        ok: bool,
        connection_id: Option<String>,
        models: Vec<String>,
        error: Option<String>,
    },
    /// The declared models of one connection.
    Models {
        connection_id: String,
        models: Vec<w::Model>,
    },
    /// The result of a connection probe.
    Checked {
        connection_id: String,
        check: w::ConnectionCheck,
    },
    /// The host's pending approvals, as refetched.
    Approvals(Vec<w::ApprovalRequest>),
    /// An approval is no longer pending.
    ApprovalStale {
        message: String,
    },
    /// An approval was resolved by this window.
    Resolved {
        request_id: String,
        rule_warning: Option<String>,
    },
    /// A companion no longer exists on the host.
    Gone {
        companion_id: String,
    },
    /// A connection's credential is registered; the field that held it clears.
    CredentialRegistered {
        connection_id: String,
    },
    /// The window should refetch the host's whole state.
    Resnapshot,
    /// A companion was created; the window opens it.
    Created(String),
    /// Current audio amplitudes for UI meters.
    AudioLevel {
        output: f32,
        input: f32,
        speaking: bool,
    },
    /// Local capture state for voice probes.
    Capture {
        level: f32,
        open: bool,
        sent: usize,
    },
    /// Current local mute flags.
    AudioMute {
        mic_muted: bool,
        output_muted: bool,
    },
    EyesEnabled(bool),
    EyesStatus(Option<String>),
    #[cfg(windows)]
    ComputerUseEnabled(bool),
    /// Live STT recognition status from `StreamAudioInput`.
    Transcription {
        text: String,
        is_final: bool,
        is_speaking: bool,
    },
    /// A companion's `MindView` loaded from `HostService.GetMind`.
    MindLoaded {
        companion_id: String,
        view: Box<w::MindView>,
    },
    /// The host's permission policy rules, refetched or updated.
    PolicyRules(Vec<w::PolicyRule>),
    /// The host's free-text permission policies, refetched or updated.
    PolicyTexts(Vec<w::PolicyText>),
    /// Avatar scale confirmed by the renderer session.
    AvatarScaleChanged(f32),
    /// Epoch- and sequence-tagged RPC response so stale replies after reconnect drop.
    Scoped {
        epoch: u64,
        seq: u64,
        inner: Box<Update>,
    },
    /// Built-in starter companion templates from the host.
    CompanionTemplates(Vec<w::CompanionTemplate>),
    /// Sanitized JSON template exported for one companion.
    CompanionExported {
        companion_id: String,
        json: String,
    },
    /// Ephemeral preview reply before saving a companion.
    CompanionPreview {
        reply: Option<String>,
        error: Option<String>,
    },
    /// Microphone capture mode / PTT state confirmed by the session (`F07`).
    CaptureModeChanged {
        mode: CaptureMode,
        ptt_active: bool,
    },
    /// Installed host plugins from `SettingsService.ListPlugins` (`F12`).
    Plugins(Vec<w::PluginInfo>),
}

/// What the window asks the session to do, because the session owns it.
#[derive(Debug)]
pub enum Command {
    /// Focus and ensure this companion is resident on this client without releasing
    /// other resident companions.
    Serve(String),
    /// Activate (mount) this companion onto this client alongside existing ones.
    ActivateCompanion(String),
    /// Release (unmount) only this companion from this client.
    ReleaseCompanion(String),
    /// Stop the current playback now.
    StopPlayback,
    /// Re-sync declared client capabilities in place.
    Refresh,
    /// Mute or unmute local microphone capture.
    SetMicMuted(bool),
    /// Switch between VAD and Push-to-Talk capture mode (`F07`).
    SetCaptureMode(CaptureMode),
    /// Begin capturing in Push-to-Talk mode (`F07`).
    PttStart,
    /// Commit the current Push-to-Talk utterance (`F07`).
    PttCommit,
    /// Cancel and discard the current Push-to-Talk utterance (`F07`).
    PttCancel,
    /// Mute or unmute local speaker playback.
    SetOutputMuted(bool),
    SetEyesEnabled(bool),
    #[cfg(windows)]
    SetComputerUseEnabled(bool),
    /// Adjust avatar size percentage (50..=200).
    SetAvatarScale(f32),
    /// Center the avatar on screen.
    CenterAvatar,
    /// Speak `text` through the host's TTS connection, optionally naming the companion.
    Speak {
        companion_id: Option<String>,
        text: String,
    },
    /// Stop serving and exit. Sent when the window closes.
    Quit,
}
