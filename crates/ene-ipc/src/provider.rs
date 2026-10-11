//! Remoc provider RPC and invocation-scoped HTTP capabilities.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "host-plugin-contracts", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "host-plugin-contracts", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "host-plugin-contracts", derive(schemars::JsonSchema))]
pub struct GenerationRequest {
    pub endpoint: String,
    pub options: Value,
    pub model: String,
    pub preamble: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "host-plugin-contracts", derive(schemars::JsonSchema))]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
}

#[cfg(feature = "provider-plugin")]
mod rpc;
#[cfg(feature = "provider-plugin")]
pub use rpc::*;

fn empty_options() -> Value {
    serde_json::json!({})
}
/// Host-enforced intermediary routing; plugin-supplied HTTP bodies cannot override it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "host-plugin-contracts", derive(schemars::JsonSchema))]
pub enum BrokerRouting {
    #[default]
    Direct,
    OpenRouter {
        only: Vec<String>,
    },
}
