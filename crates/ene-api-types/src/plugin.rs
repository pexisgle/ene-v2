//! Non-secret plugin management DTOs, shared by host and future desktop clients.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Host,
    Desktop,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Extism,
    Luau,
    Process,
    ProviderRemoc,
}
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    Llm,
    Tts,
    Stt,
    Character,
    Harness,
    Desktop,
}
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
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
pub struct EntryInfo {
    pub id: String,
    pub target: Target,
    pub runtime: Runtime,
    pub provides: Vec<Extension>,
    pub uses: Vec<Service>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub revision: String,
    pub state: String,
    pub error: Option<String>,
    pub entrypoints: Vec<EntryInfo>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEnabled {
    pub enabled: bool,
    pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    pub id: String,
    pub plugin_id: String,
    pub entrypoint: String,
    pub revision: String,
    pub companion_id: String,
    pub services: Vec<Service>,
    #[serde(default)]
    pub configuration: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInvocation {
    pub binding_id: String,
    pub operation: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginResult {
    pub generation: u64,
    pub events: Vec<Value>,
    pub result: Value,
}
