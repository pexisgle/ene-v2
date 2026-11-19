//! Connections and settings: LLM, TTS, STT, embedding, MCP and ACP share one
//! header and differ only in their config oneof (docs/api.md §7).
use super::error::{Failure, HostError, Result};
use super::model::agent_platform;
use crate::host::providers::Providers;
use ene_plugin::{Extension, provider::AuthProfile};
use ene_rpc as w;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionKind {
    Llm,
    Tts,
    Stt,
    Embedding,
    Mcp,
    Acp,
    Search,
}

impl ConnectionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::Embedding => "embedding",
            Self::Mcp => "mcp",
            Self::Acp => "acp",
            Self::Search => "search",
        }
    }
}

/// The common part of every connection config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    /// JSON Schema object text for `options`.
    #[serde(default)]
    pub(crate) options_schema: String,
    /// JSON object text conforming to `options_schema`.
    #[serde(default)]
    pub(crate) options: String,
}

impl Endpoint {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.adapter.trim().is_empty() || self.adapter.chars().count() > 64 {
            return Err(HostError::Invalid(
                ("connection adapter is required").into(),
            ));
        }
        if self.endpoint.chars().count() > 2_048 {
            return Err(HostError::Invalid(
                ("connection endpoint is too long").into(),
            ));
        }
        if !self.endpoint.trim().is_empty() {
            let url = url::Url::parse(&self.endpoint)
                .map_err(|_| HostError::Invalid(("connection endpoint must be a URL").into()))?;
            if !matches!(url.scheme(), "https" | "http") {
                return Err(HostError::Invalid(
                    ("connection endpoint must be http(s)").into(),
                ));
            }
        }
        let schema = super::json::parse_schema(&self.options_schema)?;
        let options = super::json::parse_object(&self.options)?;
        super::json::validate(&schema, &options)
            .map_err(|error| HostError::Invalid(format!("connection options: {error}")))?;
        Ok(())
    }
}

/// The six kind-specific connection bodies: an endpoint plus the fields the
/// kind adds. The oneof, its kind and its endpoint come from the same list.
macro_rules! connection_bodies {
    ($($variant:ident($name:ident) { $($field:ident: $ty:ty),* $(,)? }),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
            pub(crate) struct $name {
                pub(crate) endpoint: Endpoint,
                $(#[serde(default)] pub(crate) $field: $ty,)*
            }
        )+

        /// The kind-specific half of a connection. The variant must agree with
        /// the stored kind; the host refuses a mismatch instead of guessing.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "config", rename_all = "snake_case")]
        pub(crate) enum ConnectionConfig {
            $($variant($name)),+
        }

        impl ConnectionConfig {
            pub(crate) fn kind(&self) -> ConnectionKind {
                match self {
                    $(Self::$variant(_) => ConnectionKind::$variant),+
                }
            }

            pub(crate) fn endpoint(&self) -> &Endpoint {
                match self {
                    $(Self::$variant(config) => &config.endpoint),+
                }
            }
        }
    };
}

connection_bodies! {
    Llm(LlmConnection) { model_ids: Vec<String> },
    Tts(TtsConnection) { voice_ids: Vec<String> },
    Stt(SttConnection) { language_hints: Vec<String> },
    Embedding(EmbeddingConnection) { model_id: String, dimensions: u32 },
    Mcp(McpConnection) {
        allowed_tools: Vec<String>,
        command: String,
        args: Vec<String>,
        cwd: String,
        env: BTreeMap<String, String>,
        headers: BTreeMap<String, String>,
        auth_header: String,
        secret_env_var: String,
    },
    Acp(AcpConnection) { command: String, args: Vec<String> },
    Search(SearchConnection) { max_results: u32 },
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && name.len() <= 128
}

impl ConnectionConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        self.endpoint().validate()?;
        match self {
            Self::Llm(config) => {
                if config.model_ids.len() > 4_096
                    || config.model_ids.iter().any(|id| id.chars().count() > 512)
                {
                    return Err(HostError::Invalid(
                        ("connection model list is too large").into(),
                    ));
                }
                if config.endpoint.adapter == agent_platform::ADAPTER {
                    agent_platform::validate(&config.endpoint, &config.model_ids)?;
                }
            }
            Self::Embedding(config) => {
                if config.model_id.chars().count() > 512 {
                    return Err(HostError::Invalid(
                        ("embedding model id is too long").into(),
                    ));
                }
                if config.dimensions > 65_536 {
                    return Err(HostError::Invalid(
                        ("embedding dimensions are too large").into(),
                    ));
                }
            }
            // An ACP connection names the harness plugin its `endpoint.adapter`
            // carries, or falls back to its legacy `command`/`args`; the
            // common endpoint validation already requires the adapter.
            Self::Acp(_) => {}
            Self::Mcp(config) => {
                if config.command.trim().is_empty() && config.endpoint.endpoint.trim().is_empty() {
                    // A connection the host cannot open would be a setting that
                    // saves and then silently contributes no tools.
                    return Err(HostError::Invalid(
                        ("MCP connections need a command or an endpoint").into(),
                    ));
                }
                if config.cwd.chars().count() > 2_048 || config.cwd.contains('\0') {
                    return Err(HostError::Invalid(
                        ("MCP working directory is invalid").into(),
                    ));
                }
                if config.env.len() > 64 || config.headers.len() > 64 {
                    return Err(HostError::Invalid(
                        ("too many MCP environment variables or headers").into(),
                    ));
                }
                for (key, value) in &config.env {
                    if key.trim().is_empty()
                        || key.contains(['=', '\0', '\r', '\n'])
                        || value.contains('\0')
                    {
                        return Err(HostError::Invalid(
                            ("MCP environment variable entry is invalid").into(),
                        ));
                    }
                }
                for (name, value) in &config.headers {
                    let trimmed = name.trim();
                    if trimmed.is_empty()
                        || reqwest::header::HeaderName::from_bytes(trimmed.as_bytes()).is_err()
                        || reqwest::header::HeaderValue::from_str(value).is_err()
                    {
                        return Err(HostError::Invalid(format!("invalid MCP header `{name}`")));
                    }
                }
                let auth_header = config.auth_header.trim();
                if !auth_header.is_empty()
                    && reqwest::header::HeaderName::from_bytes(auth_header.as_bytes()).is_err()
                {
                    return Err(HostError::Invalid(
                        ("MCP auth_header must be a valid HTTP header name").into(),
                    ));
                }
                let secret_env_var = config.secret_env_var.trim();
                if !secret_env_var.is_empty() && !is_valid_env_name(secret_env_var) {
                    return Err(HostError::Invalid(
                        ("MCP secret_env_var must be a valid environment variable name").into(),
                    ));
                }
            }
            Self::Search(config) => {
                let adapter = config.endpoint.adapter.trim();
                if !matches!(adapter, "mwmbl" | "searxng" | "brave" | "tavily") {
                    return Err(HostError::Invalid(format!(
                        "unsupported search adapter `{adapter}`"
                    )));
                }
                if adapter == "searxng" && config.endpoint.endpoint.trim().is_empty() {
                    return Err(HostError::Invalid(
                        ("SearXNG search connections require an endpoint URL").into(),
                    ));
                }
                if config.max_results > 20 {
                    return Err(HostError::Invalid(
                        ("search max_results must be between 1 and 20").into(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) healthy: bool,
    pub(crate) checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure: Option<Failure>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) resources: Vec<String>,
}

/// Whether a secret is stored. `registered` secrets live in the host's
/// credential store and are not part of any connection read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialState {
    #[default]
    Absent,
    Registered,
    Environment,
}

impl CredentialState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Registered => "registered",
            Self::Environment => "environment",
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        Self::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(text))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Connection {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) kind: ConnectionKind,
    pub(crate) config: ConnectionConfig,
    pub(crate) credential_state: CredentialState,
    pub(crate) revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_check: Option<Check>,
}

impl Connection {
    pub(crate) fn validate(&self) -> Result<()> {
        super::db::validate_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.chars().count() > 128 {
            return Err(HostError::Invalid(
                ("connection name must be 1-128 characters").into(),
            ));
        }
        if self.config.kind() != self.kind {
            return Err(HostError::Invalid(
                ("connection kind does not match the config it carries").into(),
            ));
        }
        self.config.validate()
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        self.config.endpoint()
    }

    pub(crate) fn environment_variable(&self, loaded: &Providers) -> Option<String> {
        provider(&self.endpoint().adapter, loaded)
            .and_then(|provider| provider.environment_variable)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub(crate) revision: u64,
    pub(crate) connections: Vec<Connection>,
    pub(crate) updated_at: String,
    /// Free-text policies: settings prose the decision engine reads, never a
    /// mechanical gate. They ride the settings view, not its revision: their
    /// own `UpdatePolicyTexts` applies them.
    pub(crate) policy_texts: Vec<super::permission::PolicyText>,
}

/// One built-in LLM provider adapter in the contract's own shape. TTS/STT/MCP/ACP
/// use connection configs. The catalogue is derived, never stored, so there is
/// no second representation to keep in step with the wire.
fn catalog_entry(id: &str, name: &str, settings: &[(&str, &str)]) -> w::LlmProvider {
    let provider = super::model::provider_of(id).expect("built-in provider is registered in Rig");
    let config = provider
        .config("")
        .expect("built-in provider has API-key config");
    w::LlmProvider {
        id: id.into(),
        name: name.into(),
        adapter: id.into(),
        endpoint: config.base_url().into(),
        settings: settings
            .iter()
            .map(|(key, label)| w::SettingField {
                key: (*key).into(),
                label: (*label).into(),
                kind: "string".into(),
                ..Default::default()
            })
            .collect(),
        requires_credentials: provider.requires_credential(),
        environment_variable: provider.api_key_envs().first().map(|name| (*name).into()),
        ..Default::default()
    }
}

pub(crate) const SAKURA_AI_ENDPOINT: &str = "https://api.ai.sakura.ad.jp/v1";
pub(crate) const SAKURA_AI_API_KEY_ENV: &str = "SAKURA_AI_API_KEY";

/// The provider catalogue the host can speak to through rig-core. It is a
/// fixed alpha list; extensions add adapters, not host code paths.
fn builtin_providers() -> Vec<w::LlmProvider> {
    vec![
        catalog_entry(
            "openai",
            "OpenAI",
            &[("base_url", "Base URL"), ("organization", "Organization")],
        ),
        catalog_entry(
            "openrouter",
            "OpenRouter",
            &[
                ("base_url", "Base URL"),
                ("site_url", "Site URL"),
                ("app_name", "App name"),
            ],
        ),
        catalog_entry("anthropic", "Anthropic", &[("base_url", "Base URL")]),
        catalog_entry("gemini", "Google Gemini", &[("base_url", "Base URL")]),
        w::LlmProvider {
            id: agent_platform::ADAPTER.into(),
            name: "Google Agent Platform（旧 Vertex AI）".into(),
            adapter: agent_platform::ADAPTER.into(),
            endpoint: agent_platform::ENDPOINT.into(),
            settings: [
                ("project_id", "プロジェクト ID"),
                ("location", "ロケーション"),
            ]
            .into_iter()
            .map(|(key, label)| w::SettingField {
                key: key.into(),
                label: label.into(),
                kind: "string".into(),
                ..Default::default()
            })
            .collect(),
            requires_credentials: true,
            ..Default::default()
        },
        w::LlmProvider {
            id: "sakura".into(),
            name: "さくらのAI Engine".into(),
            adapter: "sakura".into(),
            endpoint: SAKURA_AI_ENDPOINT.into(),
            settings: vec![w::SettingField {
                key: "base_url".into(),
                label: "Base URL".into(),
                kind: "string".into(),
                ..Default::default()
            }],
            requires_credentials: true,
            environment_variable: Some(SAKURA_AI_API_KEY_ENV.into()),
            ..Default::default()
        },
    ]
}

/// Every provider a connection can name: the built-ins no loaded plugin
/// shadows, then one entry per loaded plugin that serves LLM connections.
///
/// A package named like a built-in replaces that built-in, because a running
/// plugin already answers for the adapter name the connection carries.
pub(crate) fn providers(loaded: &Providers) -> Vec<w::LlmProvider> {
    let mut adapters: Vec<&str> = loaded.adapters().collect();
    // The registry is a hash map; sorting keeps the catalogue the same on
    // every read, in this process and the next.
    adapters.sort_unstable();
    let plugins: Vec<w::LlmProvider> = adapters
        .into_iter()
        .filter_map(|adapter| plugin_entry(adapter, loaded))
        .collect();
    let mut entries: Vec<w::LlmProvider> = builtin_providers()
        .into_iter()
        .filter(|builtin| {
            !plugins
                .iter()
                .any(|plugin| plugin.adapter == builtin.adapter)
        })
        .collect();
    entries.extend(plugins);
    entries
}

/// One loaded plugin in the contract's own shape, when it serves LLM
/// connections.
fn plugin_entry(adapter: &str, loaded: &Providers) -> Option<w::LlmProvider> {
    let plugin = loaded.get(adapter)?;
    let (_, provider) = plugin.entry(Extension::Llm)?;
    Some(w::LlmProvider {
        id: provider.id.clone(),
        name: plugin.package.manifest.name.clone(),
        adapter: adapter.into(),
        endpoint: provider.endpoint.clone(),
        settings: provider.settings.clone(),
        requires_credentials: !matches!(&provider.auth, AuthProfile::None),
        environment_variable: provider
            .environment_variable
            .clone()
            .filter(|name| !name.is_empty()),
        ..Default::default()
    })
}

pub(crate) fn provider(adapter: &str, loaded: &Providers) -> Option<w::LlmProvider> {
    plugin_entry(adapter, loaded).or_else(|| {
        builtin_providers()
            .into_iter()
            .find(|provider| provider.adapter == adapter)
    })
}

/// The endpoint a connection calls: its configured one without a trailing
/// slash, or the provider's default when the configuration leaves it empty.
pub(crate) fn endpoint_of(connection: &Connection, loaded: &Providers) -> Option<String> {
    let configured = connection.endpoint().endpoint.trim().trim_end_matches('/');
    if !configured.is_empty() {
        return Some(configured.to_owned());
    }
    provider(&connection.endpoint().adapter, loaded)
        .map(|provider| provider.endpoint)
        .filter(|endpoint| !endpoint.is_empty())
}
