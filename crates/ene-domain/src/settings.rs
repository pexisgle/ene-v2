//! Connections and settings domain models and validation.

use crate::{ConnectionId, Failure, error::DomainError, json};
use ene_catalog::{GOOGLE_AGENT_PLATFORM, GOOGLE_AGENT_PLATFORM_ENDPOINT};
use ene_policy::PolicyText;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use strum::{EnumString, IntoStaticStr};

mod mcp;

/// Supported categories of configured external connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ConnectionKind {
    /// Chat and other text generation.
    Llm,
    /// Text to speech.
    Tts,
    /// Speech to text.
    Stt,
    /// Vector embeddings.
    Embedding,
    /// A Model Context Protocol server.
    Mcp,
    /// An external Agent Client Protocol agent.
    Acp,
    /// A web search backend.
    Search,
}

impl ConnectionKind {
    /// Returns the canonical snake-case wire/database representation.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The common part of every connection config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Adapter name that selects the transport, such as `openai`.
    pub adapter: String,
    /// Base URL, empty when the adapter derives one.
    pub endpoint: String,
    /// JSON Schema object text for `options`.
    #[serde(default)]
    pub options_schema: String,
    /// JSON object text conforming to `options_schema`.
    #[serde(default)]
    pub options: String,
}
impl Endpoint {
    /// Validates adapter name, URL syntax, and schema-checked options.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.adapter.trim().is_empty() || self.adapter.chars().count() > 64 {
            return Err(DomainError::Invalid(
                "connection adapter is required".into(),
            ));
        }
        if self.endpoint.chars().count() > 2_048 {
            return Err(DomainError::Invalid(
                "connection endpoint is too long".into(),
            ));
        }
        if !self.endpoint.trim().is_empty() {
            let url = url::Url::parse(&self.endpoint)
                .map_err(|_| DomainError::Invalid("connection endpoint must be a URL".into()))?;
            if !matches!(url.scheme(), "https" | "http") {
                return Err(DomainError::Invalid(
                    "connection endpoint must be http(s)".into(),
                ));
            }
        }
        let schema = json::parse_schema(&self.options_schema)?;
        let options = json::parse_object(&self.options)?;
        json::validate(&schema, &options)
            .map_err(|error| DomainError::Invalid(format!("connection options: {error}")))?;
        Ok(())
    }
}

/// The kind-specific connection bodies: an endpoint plus the fields the
/// kind adds. The oneof, its kind, and its endpoint come from the same list.
macro_rules! connection_bodies {
    ($($variant:ident($name:ident) = $variant_doc:literal {
        $($field:ident: $ty:ty = $field_doc:literal),* $(,)?
    }),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
            #[doc = $variant_doc]
            pub struct $name<E = Endpoint> {
                /// The shared adapter, URL and options block.
                pub endpoint: E,
                $(
                    #[serde(default)]
                    #[doc = $field_doc]
                    pub $field: $ty,
                )*
            }
        )+

        /// The kind-specific half of a connection. The variant must agree with
        /// the stored kind; callers refuse a mismatch instead of guessing.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "config", rename_all = "snake_case")]
        pub enum ConnectionConfig<E = Endpoint> {
            $(
                #[doc = $variant_doc]
                $variant($name<E>),
            )+
        }

        impl<E> ConnectionConfig<E> {
            /// Returns the connection kind corresponding to this config variant.
            pub fn kind(&self) -> ConnectionKind {
                match self {
                    $(Self::$variant(_) => ConnectionKind::$variant),+
                }
            }

            /// Returns the shared endpoint block carried by this config variant.
            pub fn endpoint(&self) -> &E {
                match self {
                    $(Self::$variant(config) => &config.endpoint),+
                }
            }

            /// Returns mutable access to the endpoint.
            pub fn endpoint_mut(&mut self) -> &mut E {
                match self {
                    $(Self::$variant(config) => &mut config.endpoint),+
                }
            }
        }
    };
}

connection_bodies! {
    Llm(LlmConnection) = "Chat and other text generation." {
        model_ids: Vec<String> = "Model ids this endpoint serves.",
    },
    Tts(TtsConnection) = "Text to speech." {
        voice_ids: Vec<String> = "Voice ids this endpoint can synthesise.",
    },
    Stt(SttConnection) = "Speech to text." {
        language_hints: Vec<String> = "Language hints for recognition; empty lets the service detect.",
    },
    Embedding(EmbeddingConnection) = "Vector embeddings." {
        model_id: String = "Embedding model to call.",
        dimensions: u32 = "Width of the returned vector, checked against the model.",
    },
    Mcp(McpConnection) = "A Model Context Protocol server." {
        allowed_tools: Vec<String> = "Tools the host may call; empty allows every tool the server lists.",
        command: String = "Executable launched for a stdio server; empty for a remote one.",
        args: Vec<String> = "Arguments passed to `command`, never a shell string.",
        cwd: String = "Working directory for `command`; empty inherits the host's.",
        env: BTreeMap<String, String> = "Environment variables added to `command`.",
        headers: BTreeMap<String, String> = "HTTP headers sent to a remote server.",
        auth_header: String = "Header name the stored secret is sent in.",
        secret_env_var: String = "Environment variable receiving the registered secret when starting a stdio server.",
    },
    Acp(AcpConnection) = "An external Agent Client Protocol agent." {
        command: String = "Executable launched for the agent.",
        args: Vec<String> = "Arguments passed to `command`.",
    },
    Search(SearchConnection) = "A web search backend." {
        max_results: u32 = "Result cap per query, at most 20.",
    },
}

fn is_valid_google_resource(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Validates a Google Agent Platform Gemini model identifier.
pub fn is_valid_agent_platform_model(id: &str) -> bool {
    id.starts_with("gemini-")
        && id.len() > "gemini-".len()
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.' | b'_')
        })
}

/// Computes the Google Agent Platform publisher base URL from `endpoint` options.
pub fn agent_platform_publisher_endpoint(endpoint: &Endpoint) -> Result<String, DomainError> {
    let base = endpoint.endpoint.trim().trim_end_matches('/');
    if !base.is_empty() && base != GOOGLE_AGENT_PLATFORM_ENDPOINT {
        return Err(DomainError::Invalid(
            "Agent Platform derives the endpoint from the Google Cloud location".into(),
        ));
    }
    let options = json::parse_object(&endpoint.options)?;
    let field = |name: &str| -> Result<&str, DomainError> {
        match options.get(name) {
            None => Ok(""),
            Some(value) => value.as_str().map(str::trim).ok_or_else(|| {
                DomainError::Invalid(format!("Agent Platform {name} must be a string"))
            }),
        }
    };
    let project = field("project_id")?;
    let location = field("location")?;
    if project.is_empty() {
        if !location.is_empty() {
            return Err(DomainError::Invalid(
                "Agent Platform requires a project ID when a location is set".into(),
            ));
        }
        return Ok(format!(
            "{GOOGLE_AGENT_PLATFORM_ENDPOINT}/v1/publishers/google/models"
        ));
    }
    let location = if location.is_empty() {
        "global"
    } else {
        location
    };
    if !is_valid_google_resource(project) || !is_valid_google_resource(location) {
        return Err(DomainError::Invalid(
            "Agent Platform project ID and location must use lowercase letters, digits or hyphens"
                .into(),
        ));
    }
    let host = match location {
        "global" => GOOGLE_AGENT_PLATFORM_ENDPOINT.into(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        _ => format!("https://{location}-aiplatform.googleapis.com"),
    };
    Ok(format!(
        "{host}/v1/projects/{project}/locations/{location}/publishers/google/models"
    ))
}

/// Validates Google Agent Platform endpoint options and model IDs.
pub fn validate_agent_platform(
    endpoint: &Endpoint,
    model_ids: &[String],
) -> Result<(), DomainError> {
    agent_platform_publisher_endpoint(endpoint)?;
    if model_ids
        .iter()
        .any(|id| !is_valid_agent_platform_model(id.trim()))
    {
        return Err(DomainError::Invalid(
            "Agent Platform model IDs must be Gemini model names such as gemini-3.8-flash".into(),
        ));
    }
    Ok(())
}

impl ConnectionConfig {
    /// Validates the endpoint and kind-specific configuration fields.
    pub fn validate(&self) -> Result<(), DomainError> {
        self.endpoint().validate()?;
        match self {
            Self::Llm(config) => {
                if config.model_ids.len() > 4_096
                    || config.model_ids.iter().any(|id| id.chars().count() > 512)
                {
                    return Err(DomainError::Invalid(
                        "connection model list is too large".into(),
                    ));
                }
                if config.endpoint.adapter == GOOGLE_AGENT_PLATFORM {
                    validate_agent_platform(&config.endpoint, &config.model_ids)?;
                }
            }
            Self::Embedding(config) => {
                if config.model_id.chars().count() > 512 {
                    return Err(DomainError::Invalid(
                        "embedding model id is too long".into(),
                    ));
                }
                if config.dimensions > 65_536 {
                    return Err(DomainError::Invalid(
                        "embedding dimensions are too large".into(),
                    ));
                }
            }
            Self::Mcp(config) => mcp::validate_mcp(config)?,
            Self::Search(config) => validate_search(config)?,
            Self::Tts(_) | Self::Stt(_) | Self::Acp(_) => {}
        }
        Ok(())
    }
}

fn validate_search(config: &SearchConnection) -> Result<(), DomainError> {
    let adapter = config.endpoint.adapter.trim().to_ascii_lowercase();
    if !matches!(
        adapter.as_str(),
        "mwmbl" | "searxng" | "brave" | "tavily" | "exa"
    ) {
        return Err(DomainError::Invalid(format!(
            "unsupported search adapter `{}`",
            config.endpoint.adapter.trim()
        )));
    }
    if adapter == "searxng" && config.endpoint.endpoint.trim().is_empty() {
        return Err(DomainError::Invalid(
            "SearXNG search connections require an endpoint URL".into(),
        ));
    }
    if config.max_results > 20 {
        return Err(DomainError::Invalid(
            "search max_results must be between 1 and 20".into(),
        ));
    }
    Ok(())
}

/// Recorded result of the last connectivity or capability check for a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Whether the last check reached the service and passed.
    pub healthy: bool,
    /// When the check ran, UTC RFC 3339 at whole-second precision.
    pub checked_at: String,
    /// Why the check failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Capabilities the service advertised, such as streaming or tool calls.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Resources the service listed, such as models or voices.
    #[serde(default)]
    pub resources: Vec<String>,
}

/// Readiness check capability for a host-loaded task harness.
pub const TASK_HARNESS_CAPABILITY: &str = "harness";

/// Whether a secret is stored. `registered` secrets live in the host's
/// credential store and are not part of any connection read.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CredentialState {
    /// No secret is stored, so the connection cannot authenticate.
    #[default]
    Absent,
    /// A secret is held in the host credential store.
    Registered,
    /// The secret comes from an environment variable named by the config.
    Environment,
}

impl CredentialState {
    /// Whether the service has a configured credential source.
    pub fn is_configured(self) -> bool {
        self != Self::Absent
    }

    /// Returns the canonical snake-case wire/database representation.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Parses a stored credential state string, falling back to `Absent` for legacy unknown tags.
    pub fn parse(text: &str) -> Self {
        text.parse().unwrap_or_default()
    }
}

/// Persisted external service connection configuration and health state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Validated ID of this connection.
    pub id: ConnectionId,
    /// Display name shown to the operator.
    pub name: String,
    /// Whether the host may use the connection.
    pub enabled: bool,
    /// Which kind of service this connection reaches.
    pub kind: ConnectionKind,
    /// Adapter, URL and kind-specific fields. Must match `kind`.
    pub config: ConnectionConfig,
    /// Whether a secret is stored for this connection.
    pub credential_state: CredentialState,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Result of the most recent health check, when one has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<Check>,
}

impl Connection {
    /// Validates connection metadata, kind alignment, and config payload.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.name.trim().is_empty() || self.name.chars().count() > 128 {
            return Err(DomainError::Invalid(
                "connection name must be 1-128 characters".into(),
            ));
        }
        if self.config.kind() != self.kind {
            return Err(DomainError::Invalid(
                "connection kind does not match the config it carries".into(),
            ));
        }
        self.config.validate()
    }

    /// Returns the endpoint definition owned by the connection's config.
    pub fn endpoint(&self) -> &Endpoint {
        self.config.endpoint()
    }
}

/// Top-level persisted settings view combining connections and free-text policies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Optimistic-concurrency counter for the settings view.
    pub revision: u64,
    /// Every configured external connection.
    pub connections: Vec<Connection>,
    /// When the view last changed, UTC RFC 3339 at whole-second precision.
    pub updated_at: String,
    /// Free-text policies: settings prose the decision engine reads, never a
    /// mechanical gate. They ride the settings view, not its revision.
    pub policy_texts: Vec<PolicyText>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm_conn(adapter: &str, endpoint: &str, options: &str, model: &str) -> Connection {
        Connection {
            id: ConnectionId::parse("conn-1").unwrap(),
            name: "Primary".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: adapter.into(),
                    endpoint: endpoint.into(),
                    options_schema: String::new(),
                    options: options.into(),
                },
                model_ids: vec![model.into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    #[test]
    fn connection_kinds_and_credential_states_keep_their_stored_spellings() {
        // Stored in SQLite and sent on the RPC wire, so every spelling is pinned.
        for (kind, text) in [
            (ConnectionKind::Llm, "llm"),
            (ConnectionKind::Tts, "tts"),
            (ConnectionKind::Stt, "stt"),
            (ConnectionKind::Embedding, "embedding"),
            (ConnectionKind::Mcp, "mcp"),
            (ConnectionKind::Acp, "acp"),
            (ConnectionKind::Search, "search"),
        ] {
            assert_eq!(kind.as_str(), text);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{text}\""));
            assert_eq!(text.parse::<ConnectionKind>().unwrap(), kind);
        }
        for (state, text) in [
            (CredentialState::Absent, "absent"),
            (CredentialState::Registered, "registered"),
            (CredentialState::Environment, "environment"),
        ] {
            assert_eq!(state.as_str(), text);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(CredentialState::parse(text), state);
        }
        // A tag this build no longer knows still decodes, to `Absent`.
        assert_eq!(
            CredentialState::parse("legacy_unknown"),
            CredentialState::Absent
        );
    }

    #[test]
    fn validates_connection_kinds_agent_platform_and_mcp_headers() {
        assert!(
            llm_conn("openai", "https://api.openai.com/v1", "{}", "gpt-5")
                .validate()
                .is_ok()
        );

        let mismatch = Connection {
            kind: ConnectionKind::Tts,
            ..llm_conn("openai", "https://api.openai.com/v1", "{}", "gpt-5")
        };
        assert!(mismatch.validate().is_err());

        let valid_ap = llm_conn(
            GOOGLE_AGENT_PLATFORM,
            "",
            r#"{"project_id":"demo-proj","location":"us-central1"}"#,
            "gemini-3.8-flash",
        );
        assert!(valid_ap.validate().is_ok());

        let invalid_ap_model = llm_conn(GOOGLE_AGENT_PLATFORM, "", "{}", "../escape");
        assert!(invalid_ap_model.validate().is_err());

        let invalid_mcp = Connection {
            id: ConnectionId::parse("mcp-1").unwrap(),
            name: "MCP".into(),
            enabled: true,
            kind: ConnectionKind::Mcp,
            config: ConnectionConfig::Mcp(McpConnection {
                endpoint: Endpoint {
                    adapter: "mcp".into(),
                    endpoint: "https://mcp.example/sse".into(),
                    ..Default::default()
                },
                headers: BTreeMap::from([("bad\nheader".into(), "value".into())]),
                ..Default::default()
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        };
        assert!(invalid_mcp.validate().is_err());
    }
}
