//! Pure domain data types shared across persistence and host modules.
#![warn(missing_docs)]
// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
mod best_effort;
mod binding;
pub mod client;
pub mod client_view;
pub mod error;
pub mod failure;
pub mod ids;
pub mod json;
pub mod schedule;
pub mod settings;
pub mod work;

pub use best_effort::{debug_if_err, warn_if_err};
pub use binding::{BodyBinding, ModelAssignment, ModelAssignments, VoiceBinding};
pub use client::{Activation, Client, DeviceOperation, DeviceStatus};
pub use ene_policy::{COMPUTER_USE, PolicyEffect, PolicyRule, PolicyText, RiskLevel};
pub use error::{DomainError, validate_id};
pub use failure::Failure;
pub use ids::{ClientId, CompanionId, SubscriptionId};
pub use settings::{
    AcpConnection, Check, Connection, ConnectionConfig, ConnectionKind, CredentialState,
    EmbeddingConnection, Endpoint, LlmConnection, McpConnection, SearchConnection, Settings,
    SttConnection, TtsConnection,
};
pub use work::{Run, RunState, WorkSession, WorkStatus};
