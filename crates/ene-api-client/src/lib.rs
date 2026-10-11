//! Generated Connect RPC clients with typed operations and bounded subscriptions.
//! SyncedSession owns snapshot buffering, event application and refreshes. A
//! caller cancelling its wait cannot interrupt synchronization. Configuration
//! workflows report confirmed progress; uncertain checks reuse one stable key.
mod audio;
mod display;
pub use display::DisplayLease;
mod events;
pub use audio::{AudioEvent, AudioInput, AudioOutput, AudioStream};
pub mod session;
pub use ene_rpc as types;
use ene_rpc::*;
pub use events::{EventStream, ServerEvent, SubscriptionInfo};
use http::header::{AUTHORIZATION, HeaderValue};
pub use session::{SessionIdentity, SyncedSession};
use std::{sync::Arc, time::Duration};
use url::Url;

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
pub use rpc::{DeviceRpcClient, HostRpcClient, PluginRpcClient};
pub use transport::{ApiClient, RuntimeConnection};

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;
