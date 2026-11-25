//! Pure domain data types shared across persistence and host modules.
#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod failure;
pub mod ids;
pub mod json;
pub mod settings;
pub mod work;

pub use client::{Activation, Client, DeviceOperation, DeviceStatus};
pub use error::{DomainError, validate_id};
pub use failure::Failure;
pub use ids::CompanionId;
pub use settings::{
    AcpConnection, Check, Connection, ConnectionConfig, ConnectionKind, CredentialState,
    EmbeddingConnection, Endpoint, LlmConnection, McpConnection, SearchConnection, Settings,
    SttConnection, TtsConnection,
};
pub use work::{Run, RunState, WorkSession, WorkStatus};
