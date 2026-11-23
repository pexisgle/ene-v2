//! The session's connection phase and its translation to the window's `Link`.
//!
//! Kept separate from `mod.rs` so the state machine reads on its own, and so
//! the serve loop stays under the file-length limit.

use super::*;

/// Where the session is in establishing or keeping the link.
///
/// One value exists at a time, so a `Live` phase cannot coexist with a
/// pending registration the way two booleans could. [`Self::to_link`] is the
/// only place a phase becomes a [`Link`], which keeps the state the window
/// shows in step with the state the session is in.
pub(super) enum ConnectionPhase {
    /// The discovery file was not written yet.
    AwaitingDiscovery { path: String },
    /// The discovery file exists but cannot be used until it changes.
    Unusable { reason: String },
    /// Registration and the first subscription are opening.
    Connecting { endpoint: String, attempt: u32 },
    /// Subscribed and receiving notifications.
    Live { version: String },
    /// The link dropped; the next attempt waits `retry_in`.
    Lost { reason: String, retry_in: Duration },
}

impl ConnectionPhase {
    /// The state the window shows for this phase.
    pub(super) fn to_link(&self) -> Link {
        match self {
            Self::AwaitingDiscovery { path } => Link::Unset {
                path: path.clone(),
                reason: "ホストがまだ接続情報を公開していません".into(),
            },
            Self::Unusable { reason } => Link::Unusable {
                reason: reason.clone(),
            },
            Self::Connecting { endpoint, attempt } => Link::Connecting {
                endpoint: endpoint.clone(),
                attempt: *attempt,
            },
            Self::Live { version } => Link::Live {
                version: version.clone(),
            },
            Self::Lost { reason, retry_in } => Link::Lost {
                reason: reason.clone(),
                retry_in: *retry_in,
            },
        }
    }
}
