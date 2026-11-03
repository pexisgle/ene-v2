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
    fn mind_view(
        &self,
        companion: String,
        now: String,
    ) -> BoxFuture<'_, Result<crate::domain::mind::StoredMind>>;
    /// One idempotent transaction for memories, current state and the
    /// operation record. A stale revision or a missing reference changes
    /// nothing.
    fn mind_update(
        &self,
        owner: String,
        operation_id: String,
        request_hash: String,
        request: crate::domain::mind::MindUpdateRequest,
        source: crate::domain::mind::SourceRef,
        now: String,
    ) -> BoxFuture<'_, Result<crate::domain::mind::MindUpdateResult>>;
    fn mind_recall(
        &self,
        owner: String,
        query: String,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<crate::domain::mind::MemoryRecord>>>;
    fn mind_read(
        &self,
        owner: String,
        id: String,
    ) -> BoxFuture<'_, Result<Option<crate::domain::mind::MemoryRecord>>>;
    fn mind_update_profile(
        &self,
        companion: String,
        expected_revision: u64,
        profile: crate::domain::mind::ProfileDocument,
        now: String,
    ) -> BoxFuture<'_, Result<(u64, crate::domain::mind::ProfileDocument)>>;
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
