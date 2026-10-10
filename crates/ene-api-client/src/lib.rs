//! Generated Connect RPC clients with a typed operations and bounded subscriptions.
//! SyncedSession owns snapshot buffering, event application and refreshes. A
//! caller cancelling its wait cannot interrupt synchronization. Configuration
//! workflows report confirmed progress; uncertain checks reuse one stable key.
mod audio;
mod display;
pub use display::DisplayLease;
mod events;
pub use audio::{AudioEvent, AudioInput, AudioOutput, AudioStream};
pub mod session;
pub use ene_api_types as types;
use ene_api_types::{
    host::*,
    provider::{Model, ProviderInfo},
};
pub use events::{EventStream, ServerEvent, SubscriptionInfo};
use http::header::{AUTHORIZATION, HeaderValue};
pub use session::{SessionIdentity, SyncedSession};
use std::{sync::Arc, time::Duration};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

mod configuration;
mod error;
mod idempotency;
pub use configuration::{
    ConfigurationComplete, ConfigurationFailure, ConfigurationIntent, ConfigurationProgress,
    ConfigurationTarget, CredentialChange,
};
mod rpc;
mod transport;
pub use error::{ApiFailure, Error};
pub use idempotency::IdempotencyKey;
pub use rpc::{DeviceRpcClient, HostRpcClient};
pub use transport::ApiClient;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;
