//! Shared LLM configuration and message types for Wasm plugins.
//! The host owns credentials and authorizes every HTTP exchange. Plugins receive
//! no credential values and cannot choose the grant/purpose attached to a job.
use serde::{Deserialize, Serialize};

/// How the host attaches a credential to a provider request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthProfile {
    /// No credential is sent.
    None,
    /// An `Authorization: Bearer` header.
    Bearer,
    /// A credential header the provider names.
    Header {
        /// Header name the credential is sent in.
        name: String,
    },
    /// A query parameter the provider names.
    Query {
        /// Parameter name the credential is sent in.
        name: String,
    },
}

/// What a provider can do beyond the single request/response shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmProviderCapabilities {
    /// Whether the provider streams its reply as it generates.
    #[serde(default)]
    pub streaming_text: bool,
    /// Whether the provider can list the models it serves.
    #[serde(default)]
    pub model_list: bool,
}

/// A model a provider offers, with the metadata the host shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// Model id passed to the provider.
    #[serde(default)]
    pub id: String,
    /// Model name shown to the operator.
    #[serde(default)]
    pub name: String,
    /// What is known about the model.
    #[serde(default)]
    pub metadata: ModelMetadata,
}

/// Where a model's metadata came from and how far it may be trusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    /// Where the metadata came from, such as a catalog or the provider itself.
    #[serde(default)]
    pub source: String,
    /// How widely the metadata applies.
    #[serde(default)]
    pub scope: String,
    /// Total context the model accepts, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Largest input the model accepts, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Largest reply the model produces, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Input kinds the model accepts, such as `text` or `image`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    /// Output kinds the model produces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    /// Request parameters the model honours.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_parameters: Vec<String>,
    /// Per-field provenance notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_sources: Vec<FieldSource>,
}

/// One `key = value` note attached to a model's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldSource {
    /// The field this note describes.
    #[serde(default)]
    pub key: String,
    /// Where that field's value came from.
    #[serde(default)]
    pub value: String,
}

/// A configuration field the host renders for this provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingField {
    /// Key the field's value is stored under.
    #[serde(default)]
    pub key: String,
    /// Label the host shows for the field.
    #[serde(default)]
    pub label: String,
    /// Input kind the host renders, such as `text` or `select`.
    #[serde(default)]
    pub kind: String,
    /// Whether the field must be filled in.
    #[serde(default)]
    pub required: bool,
    /// Allowed values, for a `select` field.
    #[serde(default)]
    pub choices: Vec<String>,
}

/// Host-enforced intermediary routing; plugin-supplied HTTP bodies cannot override it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerRouting {
    /// Send the request straight to the provider endpoint.
    #[default]
    Direct,
    /// Route through OpenRouter, limited to the listed upstreams.
    OpenRouter {
        /// Upstream provider ids the request may be routed to.
        only: Vec<String>,
    },
}
