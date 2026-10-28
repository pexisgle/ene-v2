//! Plugin package manifests and capability identifiers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Shared compatibility version for manifests, extension APIs and worker messages.
pub const CONTRACT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Host,
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Extism,
    Luau,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    Llm,
    Tts,
    Stt,
    Character,
    Harness,
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Log,
    Configuration,
    State,
    Http,
    Models,
    Tools,
    CharacterActions,
    Desktop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub contract_version: u32,
    pub id: String,
    pub name: String,
    /// Release version of this plugin, independent of the shared contract version.
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub entrypoints: Vec<EntryPoint>,
    /// All executable modules, including each entrypoint; assets have a separate budget.
    pub code_files: Vec<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub id: String,
    pub target: Target,
    pub runtime: Runtime,
    /// Relative file inside the verified code snapshot; not a host path.
    pub file: String,
    pub provides: Vec<Extension>,
    #[serde(default)]
    pub uses: Vec<Service>,
    #[serde(default)]
    pub provider: Option<ProviderConfiguration>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfiguration {
    pub id: String,
    pub endpoint: String,
    #[serde(default = "object")]
    pub options: Value,
    #[serde(default)]
    pub settings: Vec<crate::provider::SettingField>,
    pub auth: crate::provider::AuthProfile,
    #[serde(default)]
    pub routing: crate::provider::BrokerRouting,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub environment_variable: Option<String>,
    pub destinations: Vec<String>,
    #[serde(default)]
    pub models: Vec<crate::provider::Model>,
}
fn object() -> Value {
    serde_json::json!({})
}
