//! Connections and settings domain models and validation.

use crate::{Failure, error::DomainError, error::validate_id, json};
use ene_catalog::{GOOGLE_AGENT_PLATFORM, GOOGLE_AGENT_PLATFORM_ENDPOINT};
use ene_policy::PolicyText;
use http::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use strum::{EnumString, IntoStaticStr};

/// Supported categories of configured external connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ConnectionKind {
    Llm,
    Tts,
    Stt,
    Embedding,
    Mcp,
    Acp,
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
    pub adapter: String,
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
    ($($variant:ident($name:ident) { $($field:ident: $ty:ty),* $(,)? }),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
            pub struct $name {
                pub endpoint: Endpoint,
                $(#[serde(default)] pub $field: $ty,)*
            }
        )+

        /// The kind-specific half of a connection. The variant must agree with
        /// the stored kind; callers refuse a mismatch instead of guessing.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "config", rename_all = "snake_case")]
        pub enum ConnectionConfig {
            $($variant($name)),+
        }

        impl ConnectionConfig {
            /// Returns the connection kind corresponding to this config variant.
            pub fn kind(&self) -> ConnectionKind {
                match self {
                    $(Self::$variant(_) => ConnectionKind::$variant),+
                }
            }

            /// Returns the shared endpoint block carried by this config variant.
            pub fn endpoint(&self) -> &Endpoint {
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
            Self::Mcp(config) => validate_mcp(config)?,
            Self::Search(config) => validate_search(config)?,
            Self::Tts(_) | Self::Stt(_) | Self::Acp(_) => {}
        }
        Ok(())
    }
}

fn validate_mcp(config: &McpConnection) -> Result<(), DomainError> {
    if config.command.trim().is_empty() && config.endpoint.endpoint.trim().is_empty() {
        return Err(DomainError::Invalid(
            "MCP connections need a command or an endpoint".into(),
        ));
    }
    if config.cwd.chars().count() > 2_048 || config.cwd.contains('\0') {
        return Err(DomainError::Invalid(
            "MCP working directory is invalid".into(),
        ));
    }
    if config.env.len() > 64 || config.headers.len() > 64 {
        return Err(DomainError::Invalid(
            "too many MCP environment variables or headers".into(),
        ));
    }
    for (key, value) in &config.env {
        if key.trim().is_empty() || key.contains(['=', '\0', '\r', '\n']) || value.contains('\0') {
            return Err(DomainError::Invalid(
                "MCP environment variable entry is invalid".into(),
            ));
        }
    }
    for (name, value) in &config.headers {
        let trimmed = name.trim();
        if trimmed.is_empty()
            || HeaderName::from_bytes(trimmed.as_bytes()).is_err()
            || HeaderValue::from_str(value).is_err()
        {
            return Err(DomainError::Invalid(format!("invalid MCP header `{name}`")));
        }
    }
    let auth_header = config.auth_header.trim();
    if !auth_header.is_empty() && HeaderName::from_bytes(auth_header.as_bytes()).is_err() {
        return Err(DomainError::Invalid(
            "MCP auth_header must be a valid HTTP header name".into(),
        ));
    }
    let secret_env_var = config.secret_env_var.trim();
    if !secret_env_var.is_empty() && !is_valid_env_name(secret_env_var) {
        return Err(DomainError::Invalid(
            "MCP secret_env_var must be a valid environment variable name".into(),
        ));
    }
    Ok(())
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
    pub healthy: bool,
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
}

/// Whether a secret is stored. `registered` secrets live in the host's
/// credential store and are not part of any connection read.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CredentialState {
    #[default]
    Absent,
    Registered,
    Environment,
}

impl CredentialState {
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
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub kind: ConnectionKind,
    pub config: ConnectionConfig,
    pub credential_state: CredentialState,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<Check>,
}

impl Connection {
    /// Validates connection metadata, kind alignment, and config payload.
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_id(&self.id)?;
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
    pub revision: u64,
    pub connections: Vec<Connection>,
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
            id: "conn-1".into(),
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
            id: "mcp-1".into(),
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
