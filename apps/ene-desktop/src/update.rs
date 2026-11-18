//! The two messages across the one boundary the client needs: the session
//! reports to the window, and the window asks the session to act.
//!
//! Nothing else crosses a thread. The window owns what it draws, the session
//! owns the socket and the devices, and each request is a single future that
//! reports its own outcome, so there is no generic event bus to keep in sync.

use crate::state::{Devices, Link};
use ene_rpc as w;

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
    /// The host accepted a stop request. The turn itself ends later, and the
    /// window says so.
    StopRequested { companion_id: String },
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
    /// An approval is no longer pending: another device resolved it, or it
    /// expired. The window refetches and stops offering the card.
    ApprovalStale { message: String },
    /// An approval was resolved by this window.
    Resolved { request_id: String },
    /// A companion no longer exists on the host.
    Gone { companion_id: String },
    /// A connection's credential is registered; the field that held it clears.
    CredentialRegistered { connection_id: String },
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
    /// Current local mute flags.
    AudioMute { mic_muted: bool, output_muted: bool },
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
    /// Avatar scale confirmed by the renderer session.
    AvatarScaleChanged(f32),
}

/// What the window asks the session to do, because the session owns it.
#[derive(Debug)]
pub enum Command {
    /// Serve this companion: release the previous one, then activate this one.
    Serve(String),
    /// StopPlayback,
    /// Stop the current playback now. This is the explicit barge-in path: the
    /// client has no echo cancellation, so it never truncates on microphone
    /// energy.
    StopPlayback,
    /// Reconnect so streams that depend on settings are reopened. The audio
    /// subscription only exists while a TTS connection is configured, and it is
    /// opened once per connection, so a connection added while the window is
    /// open is invisible until the session reconnects.
    Refresh,
    /// Mute or unmute local microphone capture.
    SetMicMuted(bool),
    /// Mute or unmute local speaker playback.
    SetOutputMuted(bool),
    /// Adjust avatar size percentage (50..=200).
    SetAvatarScale(f32),
    /// Center the avatar on screen.
    CenterAvatar,
    /// Stop serving and exit. Sent when the window closes.
    Quit,
}
