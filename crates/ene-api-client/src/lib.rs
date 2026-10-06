//! Typed REST API v1 client with authenticated, bounded event subscriptions.
//! SyncedSession owns snapshot buffering, event application and refreshes. A
//! caller cancelling its wait cannot interrupt synchronization. Configuration
//! workflows report confirmed progress; uncertain checks reuse one stable key.
mod events;
pub mod session;
pub use ene_api_types as types;
use ene_api_types::{
    host::*,
    provider::{Model, ProviderInfo},
};
pub use events::{EventStream, ServerEvent};
use reqwest::{
    Method, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
pub use session::{SessionIdentity, SyncedSession};
use std::{sync::Arc, time::Duration};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

mod configuration;
mod error;
mod idempotency;
mod resources;
pub use configuration::{
    ConfigurationComplete, ConfigurationFailure, ConfigurationIntent, ConfigurationProgress,
    ConfigurationTarget, CredentialChange,
};
mod transport;
pub use error::{ApiFailure, Error};
pub use idempotency::IdempotencyKey;
pub use transport::ApiClient;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;
