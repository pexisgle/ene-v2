//! The selected conversation and its unacknowledged inputs.

use crate::domain::{HostCursor, Message, TurnEvent};
use ene_domain::CompanionId;

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
    /// Position of the last committed message received on the live stream.
    pub latest_live_message: Option<HostCursor>,
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
