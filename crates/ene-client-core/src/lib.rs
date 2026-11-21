//! Headless client state, update/command protocol, notification reducer, and view-model helpers.
pub mod choices;
pub mod reducer;
pub mod state;
pub mod update;

#[cfg(test)]
mod choices_tests;
#[cfg(test)]
mod state_tests;

pub use choices::ModelRoleSlot;
pub use reducer::{is_user_facing_message, merge_message, upsert_approval};
pub use state::{
    ApprovalState, AudioState, Capture, Chat, CompanionState, ConnectionViewState, Device, Devices,
    Effects, Link, LiveTranscription, Model, Notice, NoticeKind, PerceptionState, Sending,
    SpeechReport, Unsent,
};
pub use update::{CaptureMode, Command, Update};
