//! Provider-neutral text generation and outbound authorization.
use super::{Result, Secret};
use crate::domain::Connection;
use futures_util::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub streaming_text: bool,
    pub model_list: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub choices: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Number,
    Boolean,
    Select,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub endpoint: String,
    pub options: Value,
    pub settings: Vec<Field>,
    pub requires_credentials: bool,
    pub environment_variable: Option<String>,
    pub destinations: Vec<String>,
    pub state: String,
    pub error: Option<String>,
    pub revision: String,
    pub capabilities: Capabilities,
    #[serde(default)]
    pub models: Vec<Model>,
}
#[derive(Clone)]
pub struct Policy {
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
