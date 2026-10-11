//! Non-secret provider descriptors shared with RPC and plugins.
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    pub kind: SettingKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(schemars::JsonSchema)]
pub enum SettingKind {
    Text,
    Number,
    Boolean,
    Select,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Model {
    pub id: String,
    pub name: String,
    /// Provider-reported information. Absence means unknown, not unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ModelMetadata>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelMetadata {
    /// URL of the API or authoritative catalog used to obtain these values.
    pub source: String,
    /// Catalog values may differ from the endpoint selected for generation.
    pub scope: MetadataScope,
    /// Published context capacity in tokens. Null means unknown, never unlimited.
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_parameters: Option<Vec<String>>,
    /// Per-field provenance when missing API values were filled from a catalog.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub field_sources: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(schemars::JsonSchema)]
pub enum MetadataScope {
    #[default]
    Catalog,
    Route,
    #[serde(untagged)]
    Unknown(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Capabilities {
    pub streaming_text: bool,
    pub model_list: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub endpoint: String,
    pub options: Value,
    pub settings: Vec<SettingField>,
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
