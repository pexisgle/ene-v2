//! Operation-level boundaries. Application code never receives a SQL connection.
pub mod inference;
pub mod plugin;
use crate::domain::*;
use futures_util::future::BoxFuture;
use serde_json::Value;
pub type Result<T> = std::result::Result<T, Error>;
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
pub trait Repository: Send + Sync {
    fn snapshot(&self) -> BoxFuture<'_, Result<StoredSnapshot>>;
    fn configuration(&self) -> BoxFuture<'_, Result<Configuration>>;
    fn commit_configuration(&self, change: ConfigurationCommit) -> BoxFuture<'_, Result<()>>;
    fn receipt(
        &self,
        key: String,
        target: String,
        request: Value,
    ) -> BoxFuture<'_, Result<Option<(Outcome, Value)>>>;
    fn reserve(&self, receipt: Receipt) -> BoxFuture<'_, Result<()>>;
    /// Completes a receipt this repository reserved as pending. The
    /// compare-and-set leaves a concurrent or repeated completion unchanged.
    fn complete_receipt(&self, receipt: Receipt) -> BoxFuture<'_, Result<()>>;
    /// Removes a pending receipt whose operation never became accepted, so
    /// the same request key can be tried again.
    fn release_receipt(
        &self,
        key: String,
        target: String,
        request: Value,
    ) -> BoxFuture<'_, Result<()>>;
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
