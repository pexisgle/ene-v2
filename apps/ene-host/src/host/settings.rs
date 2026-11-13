//! Connections and settings: LLM, TTS, STT, embedding, MCP and ACP share one
//! header and differ only in their config oneof (docs/api.md §7).
use super::error::{Failure, HostError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionKind {
    Llm,
    Tts,
    Stt,
    Embedding,
    Mcp,
    Acp,
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
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "llm" => Self::Llm,
            "tts" => Self::Tts,
            "stt" => Self::Stt,
            "embedding" => Self::Embedding,
            "mcp" => Self::Mcp,
            "acp" => Self::Acp,
            _ => return None,
        })
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
            return Err(HostError::invalid("connection adapter is required"));
        }
        if self.endpoint.chars().count() > 2_048 {
            return Err(HostError::invalid("connection endpoint is too long"));
        }
        if !self.endpoint.trim().is_empty() {
            let url = url::Url::parse(&self.endpoint)
                .map_err(|_| HostError::invalid("connection endpoint must be a URL"))?;
            if !matches!(url.scheme(), "https" | "http") {
                return Err(HostError::invalid("connection endpoint must be http(s)"));
            }
        }
        let schema = super::json::parse_schema(&self.options_schema)?;
        let options = super::json::parse_object(&self.options)?;
        super::json::validate(&schema, &options)
            .map_err(|error| HostError::invalid(format!("connection options: {error}")))?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LlmConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) model_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TtsConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) voice_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SttConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) language_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) model_id: String,
    #[serde(default)]
    pub(crate) dimensions: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) allowed_tools: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AcpConnection {
    pub(crate) endpoint: Endpoint,
    #[serde(default)]
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
}

/// The kind-specific half of a connection. The variant must agree with the
/// stored kind; the host refuses a mismatch instead of guessing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "config", rename_all = "snake_case")]
pub(crate) enum ConnectionConfig {
    Llm(LlmConnection),
    Tts(TtsConnection),
    Stt(SttConnection),
    Embedding(EmbeddingConnection),
    Mcp(McpConnection),
    Acp(AcpConnection),
}

impl ConnectionConfig {
    pub(crate) fn kind(&self) -> ConnectionKind {
        match self {
            Self::Llm(_) => ConnectionKind::Llm,
            Self::Tts(_) => ConnectionKind::Tts,
            Self::Stt(_) => ConnectionKind::Stt,
            Self::Embedding(_) => ConnectionKind::Embedding,
            Self::Mcp(_) => ConnectionKind::Mcp,
            Self::Acp(_) => ConnectionKind::Acp,
        }
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        match self {
            Self::Llm(config) => &config.endpoint,
            Self::Tts(config) => &config.endpoint,
            Self::Stt(config) => &config.endpoint,
            Self::Embedding(config) => &config.endpoint,
            Self::Mcp(config) => &config.endpoint,
            Self::Acp(config) => &config.endpoint,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.endpoint().validate()?;
        match self {
            Self::Llm(config) => {
                if config.model_ids.len() > 4_096
                    || config.model_ids.iter().any(|id| id.chars().count() > 512)
                {
                    return Err(HostError::invalid("connection model list is too large"));
                }
            }
            Self::Embedding(config) => {
                if config.model_id.chars().count() > 512 {
                    return Err(HostError::invalid("embedding model id is too long"));
                }
                if config.dimensions > 65_536 {
                    return Err(HostError::invalid("embedding dimensions are too large"));
                }
            }
            Self::Acp(config) if config.command.trim().is_empty() => {
                return Err(HostError::invalid("ACP connections need a command"));
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
        match text {
            "registered" => Self::Registered,
            "environment" => Self::Environment,
            _ => Self::Absent,
        }
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
            return Err(HostError::invalid(
                "connection name must be 1-128 characters",
            ));
        }
        if self.config.kind() != self.kind {
            return Err(HostError::invalid(
                "connection kind does not match the config it carries",
            ));
        }
        self.config.validate()
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        self.config.endpoint()
    }

    /// The place the provider reads its secret from, per adapter.
    pub(crate) fn environment_variable(&self) -> Option<&'static str> {
        match self.endpoint().adapter.as_str() {
            "openai" => Some("OPENAI_API_KEY"),
            "openrouter" => Some("OPENROUTER_API_KEY"),
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            "gemini" => Some("GEMINI_API_KEY"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub(crate) revision: u64,
    pub(crate) connections: Vec<Connection>,
    pub(crate) updated_at: String,
}

/// A field the settings UI shows for a provider adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SettingField {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) kind: String,
    pub(crate) required: bool,
    pub(crate) choices: Vec<String>,
}

/// One built-in LLM provider adapter. TTS/STT/MCP/ACP use connection configs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Provider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) adapter: String,
    pub(crate) endpoint: String,
    pub(crate) settings: Vec<SettingField>,
    pub(crate) requires_credentials: bool,
    pub(crate) environment_variable: Option<String>,
}

fn field(key: &str, label: &str, kind: &str, required: bool) -> SettingField {
    SettingField {
        key: key.into(),
        label: label.into(),
        kind: kind.into(),
        required,
        choices: Vec::new(),
    }
}

/// The provider catalogue the host can speak to through rig-core. It is a
/// fixed alpha list; extensions add adapters, not host code paths.
pub(crate) fn providers() -> Vec<Provider> {
    vec![
        Provider {
            id: "openai".into(),
            name: "OpenAI".into(),
            adapter: "openai".into(),
            endpoint: "https://api.openai.com/v1".into(),
            settings: vec![
                field("base_url", "Base URL", "string", false),
                field("organization", "Organization", "string", false),
            ],
            requires_credentials: true,
            environment_variable: Some("OPENAI_API_KEY".into()),
        },
        Provider {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            adapter: "openrouter".into(),
            endpoint: "https://openrouter.ai/api/v1".into(),
            settings: vec![
                field("base_url", "Base URL", "string", false),
                field("site_url", "Site URL", "string", false),
                field("app_name", "App name", "string", false),
            ],
            requires_credentials: true,
            environment_variable: Some("OPENROUTER_API_KEY".into()),
        },
        Provider {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            adapter: "anthropic".into(),
            endpoint: "https://api.anthropic.com".into(),
            settings: vec![field("base_url", "Base URL", "string", false)],
            requires_credentials: true,
            environment_variable: Some("ANTHROPIC_API_KEY".into()),
        },
        Provider {
            id: "gemini".into(),
            name: "Google Gemini".into(),
            adapter: "gemini".into(),
            endpoint: "https://generativelanguage.googleapis.com".into(),
            settings: vec![field("base_url", "Base URL", "string", false)],
            requires_credentials: true,
            environment_variable: Some("GEMINI_API_KEY".into()),
        },
    ]
}

pub(crate) fn provider(adapter: &str) -> Option<Provider> {
    providers()
        .into_iter()
        .find(|provider| provider.adapter == adapter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            adapter: "openai".into(),
            endpoint: "https://api.openai.com/v1".into(),
            options_schema: r#"{"type":"object"}"#.into(),
            options: "{}".into(),
        }
    }

    #[test]
    fn connection_kind_must_match_its_config() {
        let connection = Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: endpoint(),
                model_ids: vec!["gpt-5".into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        };
        connection.validate().unwrap();
        let mut wrong = connection.clone();
        wrong.kind = ConnectionKind::Tts;
        assert!(wrong.validate().is_err());
        assert_eq!(connection.environment_variable(), Some("OPENAI_API_KEY"));

        let mut bad_options = connection.clone();
        bad_options.config = ConnectionConfig::Llm(LlmConnection {
            endpoint: Endpoint {
                options_schema: r#"{"type":"object","required":["region"],"properties":{"region":{"type":"string"}}}"#.into(),
                options: "{}".into(),
                ..endpoint()
            },
            model_ids: Vec::new(),
        });
        assert!(bad_options.validate().is_err());
    }

    #[test]
    fn credential_states_and_kinds_round_trip() {
        for state in [
            CredentialState::Absent,
            CredentialState::Registered,
            CredentialState::Environment,
        ] {
            assert_eq!(CredentialState::parse(state.as_str()), state);
        }
        for kind in [
            ConnectionKind::Llm,
            ConnectionKind::Tts,
            ConnectionKind::Stt,
            ConnectionKind::Embedding,
            ConnectionKind::Mcp,
            ConnectionKind::Acp,
        ] {
            assert_eq!(ConnectionKind::parse(kind.as_str()), Some(kind));
        }
    }
}
