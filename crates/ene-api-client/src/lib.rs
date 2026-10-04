//! Typed REST API v1 client with authenticated, bounded event subscriptions.
//! No implicit mutation retries: retain the same `IdempotencyKey` and input when
//! an acceptance is uncertain. Subscribe, fetch a snapshot, then apply buffered
//! notifications with `ene_api_types::sync::Replica`.
mod events;
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
use std::{sync::Arc, time::Duration};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

mod error;
mod idempotency;
mod resources;
mod transport;
pub use error::{ApiFailure, Error};
pub use idempotency::IdempotencyKey;
pub use transport::ApiClient;

#[cfg(test)]
mod tests;
