//! Shared LLM configuration and message types for Wasm plugins.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthProfile {
    None,
    Bearer,
    Header { name: String },
    Query { name: String },
}

/// What a provider can do beyond the single request/response shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmProviderCapabilities {
    #[serde(default)]
    pub streaming_text: bool,
    #[serde(default)]
    pub model_list: bool,
}

/// A model a provider offers, with the metadata the host shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub metadata: ModelMetadata,
}

/// Where a model's metadata came from and how far it may be trusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_parameters: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_sources: Vec<FieldSource>,
}

/// One `key = value` note attached to a model's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldSource {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// A configuration field the host renders for this provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingField {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub choices: Vec<String>,
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
