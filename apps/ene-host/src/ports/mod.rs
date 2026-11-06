//! Operation-level boundaries. Application code never receives a SQL connection.
pub mod inference;
pub mod plugin;
use crate::domain::*;
use futures_util::future::BoxFuture;
use serde_json::Value;
pub type Result<T> = std::result::Result<T, Error>;
/// The result of a request a receipt records. A success is already the type
/// the API returns; anything else is the body the receipt holds, which only
/// the API boundary decides how to read.
pub enum Answer<T> {
    Success(T),
    Stored(Outcome, Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Completed,
    Created,
    Accepted,
    Unavailable,
}
#[derive(Clone)]
pub struct Receipt {
    pub key: String,
    pub target: String,
    pub request: Value,
    pub outcome: Outcome,
    pub response: Value,
}
/// Configuration and receipt effect are one durable transaction.
pub enum ConfigurationCommit {
    Update(Configuration),
    CreateGrant {
        configuration: Configuration,
        receipt: Receipt,
    },
    CompleteCheck {
        configuration: Configuration,
        receipt: Receipt,
    },
}
/// Secrets have no Debug/Serialize implementation and never become protocol messages.
#[derive(Clone)]
pub struct Secret {
    pub value: String,
    pub nonce: String,
}
pub trait Credentials: Send + Sync {
    fn environment(&self, name: &str) -> Result<String>;
    fn session_only(&self) -> bool;
    fn load(&self, reference: String) -> BoxFuture<'_, Result<String>>;
    fn save(&self, reference: String, value: String) -> BoxFuture<'_, Result<()>>;
    fn forget(&self, reference: String) -> BoxFuture<'_, Result<()>>;
}
