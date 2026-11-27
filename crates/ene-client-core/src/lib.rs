//! Headless client state, domain ACL, reducer, command protocol, and connection helpers.
#![warn(missing_docs)]

pub mod choices;
pub mod connections;
pub mod presets;
pub mod reducer;
pub mod state;
pub mod templates;
pub mod update;

mod attachments;
mod notifications;

#[cfg(test)]
mod choices_tests;
#[cfg(test)]
mod state_tests;

pub use attachments::is_attachment_image;
pub use choices::ModelRoleSlot;
pub use ene_catalog::{
    GOOGLE_AGENT_PLATFORM, SAKURA_AI_ENDPOINT, SearchAdapter, VOICEVOX_DEFAULT_ENDPOINT,
    default_embedding_dimensions, is_sakura_endpoint, parse_voice_resource,
    search_adapter_requires_key,
};
pub use ene_domain::client_view as domain;
pub use reducer::{is_user_facing_message, merge_message, upsert_approval};
pub use state::{
    AudioState, Capture, Chat, CompanionState, ConnectionViewState, Device, Devices, Effects, Link,
    LiveTranscription, Model, Notice, NoticeKind, PerceptionState, Sending, SpeechReport, Unsent,
};
pub use update::{CaptureMode, Command, Update};
