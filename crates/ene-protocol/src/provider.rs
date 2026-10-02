//! Provider plugin protocol v1: stdout is bounded NDJSON; diagnostics use stderr.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_LINE: usize = 1024 * 1024;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Initialize {
        request_id: String,
        protocol_version: u32,
    },
    /// Discover current models and optional metadata through the host HTTP broker.
    /// Unknown limits/modalities must be omitted; never infer them from model IDs.
    ListModels {
        request_id: String,
        endpoint: String,
        options: Value,
    },
    Generate {
        request_id: String,
        request: GenerationRequest,
    },
    HttpResponse {
        request_id: String,
        http_id: String,
        status: u16,
        headers: BTreeMap<String, String>,
        data_base64: String,
        done: bool,
    },
    Cancel {
        request_id: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginMessage {
    Initialized {
        request_id: String,
        protocol_version: u32,
        capabilities: Capabilities,
    },
    Models {
        request_id: String,
        models: Vec<Model>,
    },
    Delta {
        request_id: String,
        text: String,
    },
    Finished {
        request_id: String,
        reason: String,
    },
    Error {
        request_id: String,
        code: String,
        message: String,
    },
    HttpRequest {
        request_id: String,
        http_id: String,
        method: String,
        url: String,
        headers: BTreeMap<String, String>,
        body_base64: Option<String>,
    },
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
