//! Provider-neutral text generation and outbound authorization.
use super::{Result, Secret};
use crate::domain::Connection;
pub use ene_api_types::provider::{
    Capabilities, Model, ProviderInfo as Descriptor, SettingField as Field,
    SettingKind as FieldKind,
};
use futures_util::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Policy {
    /// Same implementation revision as the public provider descriptor.
    pub revision: String,
    pub fingerprint: Value,
    pub destinations: Vec<String>,
    pub requires_credentials: bool,
    pub environment_variable: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Generation {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub preamble: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u64,
}
mod budget;
pub use budget::MAX_GENERATION_BYTES;
#[derive(Clone, Debug)]
pub enum Event {
    Delta(String),
    Finished(String),
}
pub trait Authorization: Send + Sync {
    fn check(&self) -> BoxFuture<'_, Result<()>>;
}
pub struct Invocation {
    pub id: String,
    pub connection: Connection,
    pub request: Generation,
    pub secret: Option<Secret>,
    pub authorization: Arc<dyn Authorization>,
    pub cancel: watch::Receiver<bool>,
}
pub trait ProviderRegistry: Send + Sync {
    fn descriptors(&self) -> Vec<Descriptor>;
    fn policy(&self, connection: &Connection) -> Result<Policy>;
    fn generate(
        &self,
        invocation: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>>;
    fn models(&self, invocation: Invocation) -> BoxFuture<'_, Result<Vec<Model>>>;
    fn reload(&self) -> BoxFuture<'_, Result<()>>;
    fn enable(&self, id: String, enabled: bool) -> BoxFuture<'_, Result<()>>;
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
/// Every builtin and external adapter implements this same provider-neutral boundary.
pub trait LlmProvider: Send + Sync {
    fn policy(&self, connection: &Connection) -> Result<Policy>;
    fn generate(
        &self,
        invocation: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>>;
    fn models(&self, invocation: Invocation) -> BoxFuture<'_, Result<Vec<Model>>>;
}
