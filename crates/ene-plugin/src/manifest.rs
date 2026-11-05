//! Plugin package manifests and capability identifiers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Shared compatibility version for manifests, extension APIs.
pub const CONTRACT_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Host,
    /// An external ACP agent launched as an explicit subprocess.
    Acp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    Llm,
    Character,
    Harness,
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub id: String,
    pub target: Target,
    /// Relative file inside the verified code snapshot; empty for `acp`.
    #[serde(default)]
    pub file: String,
    /// Executable and arguments for `acp` entrypoints; never a shell command.
    #[serde(default)]
    pub command: Vec<String>,
    pub provides: Vec<Extension>,
    #[serde(default)]
    pub uses: Vec<Service>,
    #[serde(default)]
    pub provider: Option<ProviderConfiguration>,
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
