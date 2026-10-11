//! Operation-level boundaries. Application code never receives a SQL connection.
pub mod llm;
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
pub enum Accepted {
    New {
        receipt: MessageAcceptance,
        user: Box<Message>,
        response: Box<Message>,
    },
    Replay {
        receipt: MessageAcceptance,
    },
}
impl Accepted {
    pub fn receipt(&self) -> &MessageAcceptance {
        match self {
            Self::New { receipt, .. } | Self::Replay { receipt } => receipt,
        }
    }
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
    fn messages(
        &self,
        c: String,
        cursor: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Page<Message>>>;
    fn message(&self, c: String, id: String) -> BoxFuture<'_, Result<Option<Message>>>;
    fn configuration(&self) -> BoxFuture<'_, Result<Configuration>>;
    fn commit_configuration(&self, change: ConfigurationCommit) -> BoxFuture<'_, Result<()>>;
    fn receipt(
        &self,
        key: String,
        target: String,
        request: Value,
    ) -> BoxFuture<'_, Result<Option<(Outcome, Value)>>>;
    fn reserve(&self, receipt: Receipt) -> BoxFuture<'_, Result<()>>;
    fn accept(
        &self,
        key: String,
        companion: String,
        request: Value,
    ) -> BoxFuture<'_, Result<Accepted>>;
    fn update_message(&self, id: String, update: MessageUpdate) -> BoxFuture<'_, Result<Message>>;
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
