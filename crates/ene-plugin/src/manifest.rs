//! Plugin package manifests and capability identifiers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Shared compatibility version for manifests, extension APIs.
pub const CONTRACT_VERSION: u32 = 8;

/// Where an entrypoint runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Server-side Wasm module executing inside the host.
    Host,
    /// Client-side desktop plugin providing device capabilities (`F11`).
    Client,
    /// An external ACP agent launched as an explicit subprocess.
    Acp,
}

/// An extension API an entrypoint can provide or use.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    /// Chat and other text generation.
    Llm,
    /// Vector embeddings.
    Embedding,
    /// Text to speech.
    Tts,
    /// Speech to text.
    Stt,
    /// Avatar expressions, motions and speech.
    Character,
    /// Task harness sessions.
    Harness,
    /// Lifecycle hooks that may block or add context.
    Hook,
    /// Tools the plugin offers the model.
    Tool,
}

impl Extension {
    /// Whether this extension is served by a provider configuration, as
    /// opposed to a client-facing behaviour such as `Character`.
    pub fn serves_provider(self) -> bool {
        matches!(self, Self::Llm | Self::Embedding | Self::Tts | Self::Stt)
    }
}

/// A host service an entrypoint may request.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    /// Write to the host log.
    Log,
    /// Read the plugin's configuration.
    Configuration,
    /// Keep a small key-value store across invocations.
    State,
    /// Perform HTTP exchanges through the broker.
    Http,
    /// Call a model assigned to the task.
    Models,
    /// Call the host's own tools.
    Tools,
}

/// A plugin package declaration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Contract version this manifest targets; must equal [`CONTRACT_VERSION`].
    pub contract_version: u32,
    /// Package id, unique among installed plugins.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Release version of this plugin, independent of the shared contract version.
    pub version: String,
    /// Operator-facing summary.
    #[serde(default)]
    pub description: String,
    /// The entrypoints this package exposes.
    pub entrypoints: Vec<EntryPoint>,
}

/// One executable entry inside a manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    /// Id of this entrypoint, unique within the manifest.
    pub id: String,
    /// Where the entrypoint runs.
    pub target: Target,
    /// Relative file inside the verified code snapshot; empty for `acp`.
    #[serde(default)]
    pub file: String,
    /// Executable and arguments for `acp` entrypoints; never a shell command.
    #[serde(default)]
    pub command: Vec<String>,
    /// Extensions this entrypoint provides.
    pub provides: Vec<Extension>,
    /// Host services this entrypoint may request.
    #[serde(default)]
    pub uses: Vec<Service>,
    /// Credential and routing configuration, for providers.
    #[serde(default)]
    pub provider: Option<ProviderConfiguration>,
}

/// How one provider is reached and authenticated.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfiguration {
    /// Provider id, unique within the plugin.
    pub id: String,
    /// Base URL of the provider.
    pub endpoint: String,
    /// Adapter-specific options.
    #[serde(default = "object")]
    pub options: Value,
    /// Configuration fields the host renders for this provider.
    #[serde(default)]
    pub settings: Vec<crate::provider::SettingField>,
    /// How the host attaches the credential.
    pub auth: crate::provider::AuthProfile,
    /// Routing the host enforces.
    #[serde(default)]
    pub routing: crate::provider::BrokerRouting,
    /// Extra headers added to every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variable the host reads the credential from.
    #[serde(default)]
    pub environment_variable: Option<String>,
    /// Destinations the provider may reach.
    pub destinations: Vec<String>,
    /// Models this provider serves.
    #[serde(default)]
    pub models: Vec<crate::provider::Model>,
    /// Which extensions this one provider serves. A provider that answers for
    /// several modalities (an OpenAI-compatible endpoint serving chat,
    /// embeddings and speech) names all of them, so a host can tell which
    /// `settings` and `models` apply to which request. Empty means the
    /// entrypoint's `provides`.
    #[serde(default)]
    pub extensions: Vec<Extension>,
}
fn object() -> Value {
    serde_json::json!({})
}
