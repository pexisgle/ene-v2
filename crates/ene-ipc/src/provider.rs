//! Remoc provider RPC and invocation-scoped HTTP capabilities.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub command: Vec<String>,
    pub endpoint: String,
    #[serde(default = "empty_options")]
    pub options: Value,
    #[serde(default)]
    pub settings: Vec<SettingField>,
    pub auth: AuthProfile,
    #[serde(default)]
    pub routing: BrokerRouting,
    /// Non-secret, host-owned headers (for example an API version).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub environment_variable: Option<String>,
    pub destinations: Vec<String>,
    /// Optional offline hints; ListModels results are authoritative when supported.
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthProfile {
    None,
    Bearer,
    Header { name: String },
    Query { name: String },
}

pub use ene_api_types::provider::{
    Capabilities, MetadataScope, Model, ModelMetadata, ProviderInfo, SettingField, SettingKind,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub preamble: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
}

/// Dynamic provider options and metadata require a self-describing codec.
/// MessagePack encodes these Serde values without a text framing protocol.
pub type Codec = remoc::codec::MessagePack;
pub type Client = ProviderClient<Codec>;
pub type BrokerClient = HttpBrokerClient<Codec>;
pub type Sender<T> = remoc::rch::mpsc::Sender<T, Codec>;

#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum ProviderError {
    #[error("provider failed")]
    Failed,
    #[error("invalid input")]
    InvalidInput,
    #[error("permission denied")]
    PermissionDenied,
    #[error("provider unavailable")]
    Unavailable,
    #[error(transparent)]
    Call(#[from] remoc::rtc::CallError),
}

/// The broker capability is scoped by the host to this invocation.
/// Dropping a call cancels it; dropping the client stops the provider server.
#[remoc::rtc::remote]
pub trait Provider {
    async fn capabilities(&self) -> Result<Capabilities, ProviderError>;
    async fn list_models(
        &self,
        endpoint: String,
        options: Value,
        broker: BrokerClient,
    ) -> Result<Vec<Model>, ProviderError>;
    /// Text arrives in order on `deltas`; the RPC result is the finish reason.
    async fn generate(
        &self,
        request: GenerationRequest,
        broker: BrokerClient,
        deltas: Sender<String>,
    ) -> Result<String, ProviderError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HttpResponsePart {
    Head {
        status: u16,
        headers: BTreeMap<String, String>,
    },
    Data(Vec<u8>),
}

/// Drain the response channel concurrently with the call to apply backpressure.
/// The RPC result reports completion or failure of the HTTP exchange.
#[remoc::rtc::remote]
pub trait HttpBroker {
    async fn request(
        &self,
        request: HttpRequest,
        response: Sender<HttpResponsePart>,
    ) -> Result<(), ProviderError>;
}

fn empty_options() -> Value {
    serde_json::json!({})
}
/// Host-enforced intermediary routing; plugin-supplied HTTP bodies cannot override it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerRouting {
    #[default]
    Direct,
    OpenRouter {
        only: Vec<String>,
    },
}
