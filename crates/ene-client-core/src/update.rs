//! The two messages across the one boundary the client needs: the session
//! reports to the window, and the window asks the session to act.

use crate::domain::{
    CompanionTemplate, HostNotification, LlmProvider, Message, MindView, ModelInfo, PluginInfo,
};
use crate::state::{Devices, Link, Notice};

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
    /// One host notification, translated to domain types, for the window to merge.
    Notification(Box<HostNotification>),
    /// The host ended the subscription and the session is resubscribing.
    Resync,
    /// A local device changed state.
    Devices(Box<Devices>),
    /// An action the operator asked for did not happen.
    Failed {
        /// The action that failed, in the user's words.
        context: &'static str,
        /// What happened, written for the operator.
        message: String,
    },
    /// A revision the window used was stale; the window refetches the authority.
    Conflict {
        /// The action that failed, in the user's words.
        context: &'static str,
        /// What happened, written for the operator.
        message: String,
    },
    /// The host refused the owner credential or the endpoint.
    Denied {
        /// The action that failed, in the user's words.
        context: &'static str,
        /// What happened, written for the operator.
        message: String,
    },
    /// A message was accepted into the conversation.
    Accepted {
        /// Client-side key that makes a retry idempotent.
        request_key: String,
        /// The approval that was answered.
        request_id: String,
    },
    /// A message was not accepted; the text stays with the operator.
    SendFailed {
        /// Client-side key that makes a retry idempotent.
        request_key: String,
        /// The text that was not accepted.
        text: String,
        /// Why the host refused it.
        reason: String,
    },
    /// The host accepted a stop request.
    StopRequested {
        /// The companion this report is about.
        companion_id: String,
    },
    /// The host synthesized and dispatched a probe utterance.
    Spoke,
    /// The probe could not be synthesized or dispatched.
    SpokeFailed {
        /// What happened, written for the operator.
        message: String,
    },
    /// The conversation the window asked for.
    History {
        /// The companion this report is about.
        companion_id: String,
        /// The conversation, oldest first.
        messages: Vec<Message>,
    },
    /// The conversation fetch failed.
    HistoryFailed {
        /// The companion the history fetch was for.
        companion_id: String,
        /// Why the fetch failed.
        message: String,
    },
    /// The provider catalog.
    Providers(Vec<LlmProvider>),
    /// The connection and any credential have been saved for onboarding.
    SetupSaved(Result<String, Notice>),
    /// The chosen model has been applied to the companion during onboarding.
    SetupApplied(Result<String, Notice>),
    /// The declared models of one connection.
    Models {
        /// The connection the models belong to.
        connection_id: String,
        /// The models the connection declares.
        models: Vec<ModelInfo>,
    },
    /// The result of a connection probe.
    Checked {
        /// The connection that was probed.
        connection_id: String,
        /// Local ownership of the pending probe, echoed by its completion.
        ticket: String,
        /// Whether the saved check was healthy, or why the request failed.
        result: Result<bool, Notice>,
    },
    /// An approval is no longer pending.
    ApprovalStale {
        /// What happened, written for the operator.
        message: String,
    },
    /// An approval was resolved by this window.
    Resolved {
        /// A warning about the saved rule, when one is worth saying.
        rule_warning: Option<String>,
    },
    /// A companion no longer exists on the host.
    Gone {
        /// The companion this report is about.
        companion_id: String,
    },
    /// A connection's credential is registered; the field that held it clears.
    CredentialRegistered {
        /// The connection whose credential was registered.
        connection_id: String,
    },
    /// The window should refetch the host's whole state.
    Resnapshot,
    /// A companion was created; the window opens it.
    Created(String),
    /// Current audio amplitudes for UI meters.
    AudioLevel {
        /// Output level, normalised.
        output: f32,
        /// Input level, normalised.
        input: f32,
        /// Whether the companion is talking right now.
        speaking: bool,
    },
    /// Local capture state for voice probes.
    Capture {
        /// Current input level, normalised.
        level: f32,
        /// Whether the capture device is open.
        open: bool,
        /// How many samples have been sent.
        sent: usize,
    },
    /// Current local mute flags.
    AudioMute {
        /// Whether the microphone is muted.
        mic_muted: bool,
        /// Whether the audio output is muted.
        output_muted: bool,
    },
    /// The local Eyes preference and native observation availability changed.
    EyesChanged(crate::state::EyesState),
    /// The perception status line, when the host reported one.
    EyesStatus(Option<String>),
    /// Enables or disables Windows desktop input for this client.
    #[cfg(windows)]
    ComputerUseEnabled(bool),
    /// Live STT recognition status from `StreamAudioInput`.
    Transcription {
        /// The text that was not accepted.
        text: String,
        /// Whether the provider considers this the final transcript.
        is_final: bool,
        /// Whether the person is speaking right now.
        is_speaking: bool,
    },
    /// A companion's `MindView` loaded from `HostService.GetMind`.
    MindLoaded {
        /// The companion this report is about.
        companion_id: String,
        /// The mind view that was loaded.
        view: Box<MindView>,
    },
    /// Avatar scale confirmed by the renderer session.
    AvatarScaleChanged(f32),
    /// Epoch- and sequence-tagged RPC response so stale replies after reconnect drop.
    Scoped {
        /// Which link the response belongs to.
        epoch: u64,
        /// Order of the response within that link.
        seq: u64,
        /// The wrapped report.
        inner: Box<Update>,
    },
    /// Built-in starter companion templates from the host.
    CompanionTemplates(Vec<CompanionTemplate>),
    /// Sanitized JSON template exported for one companion.
    CompanionExported {
        /// The companion this report is about.
        companion_id: String,
        /// The exported template JSON.
        json: String,
    },
    /// Ephemeral preview reply before saving a companion.
    CompanionPreview {
        /// The preview reply, when it succeeded.
        reply: Option<String>,
        /// Why the preview failed, when it did.
        error: Option<String>,
    },
    /// Microphone capture mode / PTT state confirmed by the session (`F07`).
    CaptureModeChanged {
        /// The capture mode now in force.
        mode: CaptureMode,
        /// Whether push-to-talk is holding the microphone open.
        ptt_active: bool,
    },
    /// Installed host plugins from `SettingsService.ListPlugins` (`F12`).
    Plugins(Vec<PluginInfo>),
}

/// A newer response can replace an older one for the same view resource.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) enum ReplyTarget {
    Providers,
    Models(String),
    Check(String),
    Credential(String),
    History(String),
    Mind(String),
    Plugins,
    Templates,
    Preview,
}

impl Update {
    pub(crate) fn reply_target(&self) -> Option<ReplyTarget> {
        Some(match self {
            Self::Providers(_) => ReplyTarget::Providers,
            Self::Models { connection_id, .. } => ReplyTarget::Models(connection_id.clone()),
            Self::Checked { connection_id, .. } => ReplyTarget::Check(connection_id.clone()),
            Self::CredentialRegistered { connection_id } => {
                ReplyTarget::Credential(connection_id.clone())
            }
            Self::History { companion_id, .. } | Self::HistoryFailed { companion_id, .. } => {
                ReplyTarget::History(companion_id.clone())
            }
            Self::MindLoaded { companion_id, .. } => ReplyTarget::Mind(companion_id.clone()),
            Self::Plugins(_) => ReplyTarget::Plugins,
            Self::CompanionTemplates(_) => ReplyTarget::Templates,
            Self::CompanionPreview { .. } => ReplyTarget::Preview,
            _ => return None,
        })
    }
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
    /// Turn screen perception on or off.
    SetEyesEnabled(bool),
    /// Allow or forbid the host driving the desktop.
    #[cfg(windows)]
    SetComputerUseEnabled(bool),
    /// Adjust avatar size percentage (50..=200).
    SetAvatarScale(f32),
    /// Center the avatar on screen.
    CenterAvatar,
    /// Speak `text` through the host's TTS connection, optionally naming the companion.
    Speak {
        /// The companion whose voice to use, when one is named.
        companion_id: Option<String>,
        /// The words to speak.
        text: String,
    },
    /// Stop serving and exit. Sent when the window closes.
    Quit,
}

impl Command {
    /// Whether this command only touches this client, so it can be applied
    /// while the link is down and the wait for a reconnect should continue.
    ///
    /// Everything else addresses a live session: applying it during an outage
    /// does nothing, and "handling" it must not cut the reconnect backoff short.
    /// A `Quit` is neither: it ends the wait entirely.
    pub fn is_local_only(&self) -> bool {
        match self {
            Self::SetEyesEnabled(_) | Self::SetMicMuted(_) | Self::SetOutputMuted(_) => true,
            #[cfg(windows)]
            Self::SetComputerUseEnabled(_) => true,
            Self::Serve(_)
            | Self::ActivateCompanion(_)
            | Self::ReleaseCompanion(_)
            | Self::StopPlayback
            | Self::Refresh
            | Self::SetCaptureMode(_)
            | Self::PttStart
            | Self::PttCommit
            | Self::PttCancel
            | Self::SetAvatarScale(_)
            | Self::CenterAvatar
            | Self::Speak { .. }
            | Self::Quit => false,
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    /// The distinction that keeps a reconnect backoff intact: a command this
    /// client can apply alone must not be treated as one that needs the host.
    #[test]
    fn only_local_state_commands_are_local() {
        for command in [
            Command::SetEyesEnabled(false),
            Command::SetMicMuted(true),
            Command::SetOutputMuted(true),
        ] {
            assert!(
                command.is_local_only(),
                "{command:?} only touches this client"
            );
        }

        for command in [
            Command::StopPlayback,
            Command::Refresh,
            Command::PttCancel,
            Command::CenterAvatar,
            Command::Serve("ene".into()),
            Command::Quit,
            Command::Speak {
                companion_id: None,
                text: "hi".into(),
            },
        ] {
            assert!(
                !command.is_local_only(),
                "{command:?} needs the live session or ends the wait"
            );
        }
    }
}
