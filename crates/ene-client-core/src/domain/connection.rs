//! Pure connection, provider, model, and plugin domain types.

use super::Failure;
use std::collections::BTreeMap;

/// Kind of host-managed external connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionKind {
    #[default]
    Unspecified,
    Llm,
    Tts,
    Stt,
    Embedding,
    Mcp,
    Acp,
    Search,
}

impl ConnectionKind {
    /// Selectable connection kinds shown in the custom connection creation form.
    pub const ALL_CUSTOM: [Self; 7] = [
        Self::Llm,
        Self::Tts,
        Self::Stt,
        Self::Embedding,
        Self::Mcp,
        Self::Acp,
        Self::Search,
    ];

    /// Short Japanese display label for this connection kind.
    pub fn label(self) -> &'static str {
        match self {
            Self::Llm => "LLM",
            Self::Tts => "音声合成",
            Self::Stt => "音声認識",
            Self::Embedding => "埋め込み",
            Self::Mcp => "MCP",
            Self::Acp => "ACP",
            Self::Search => "Web検索",
            Self::Unspecified => "その他",
        }
    }

    /// Form field label for the kind-specific list/value input.
    pub fn list_field_label(self) -> &'static str {
        match self {
            Self::Llm => "使用するモデルID（カンマ区切り・空欄で自動）",
            Self::Tts => "話者IDリスト（カンマ区切り）",
            Self::Stt => "言語ヒント（例: ja, en）",
            Self::Embedding => "埋め込みモデルID",
            Self::Mcp => "許可ツール（カンマ区切り・空欄で全件）",
            Self::Acp => "起動引数（スペース区切り）",
            Self::Search => "最大検索結果数 (1–20)",
            Self::Unspecified => "オプション",
        }
    }
}

/// Endpoint adapter, URL, and JSON options schema/payload for a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionEndpoint {
    pub adapter: String,
    pub endpoint: String,
    pub options_schema: String,
    pub options: String,
}

/// LLM connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub model_ids: Vec<String>,
}

/// Text-to-speech connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TtsConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub voice_ids: Vec<String>,
}

/// Speech-to-text connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SttConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub language_hints: Vec<String>,
}

/// Text embedding connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbeddingConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub model_id: String,
    pub dimensions: u32,
}

/// Model Context Protocol (MCP) server connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub allowed_tools: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub auth_header: String,
    pub secret_env_var: String,
}

/// Agent Client Protocol (ACP) harness connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub command: String,
    pub args: Vec<String>,
}

/// Web search provider connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchConnection {
    pub endpoint: Option<ConnectionEndpoint>,
    pub max_results: u32,
}

/// Kind-specific connection configuration payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionConfig {
    Llm(LlmConnection),
    Tts(TtsConnection),
    Stt(SttConnection),
    Embedding(EmbeddingConnection),
    Mcp(McpConnection),
    Acp(AcpConnection),
    Search(SearchConnection),
}

impl ConnectionConfig {
    /// Returns the endpoint metadata inside whichever config variant is present.
    pub fn endpoint(&self) -> Option<&ConnectionEndpoint> {
        match self {
            Self::Llm(c) => c.endpoint.as_ref(),
            Self::Tts(c) => c.endpoint.as_ref(),
            Self::Stt(c) => c.endpoint.as_ref(),
            Self::Embedding(c) => c.endpoint.as_ref(),
            Self::Mcp(c) => c.endpoint.as_ref(),
            Self::Acp(c) => c.endpoint.as_ref(),
            Self::Search(c) => c.endpoint.as_ref(),
        }
    }

    /// Returns a mutable reference to the endpoint metadata inside this config.
    pub fn endpoint_mut(&mut self) -> Option<&mut ConnectionEndpoint> {
        match self {
            Self::Llm(c) => c.endpoint.as_mut(),
            Self::Tts(c) => c.endpoint.as_mut(),
            Self::Stt(c) => c.endpoint.as_mut(),
            Self::Embedding(c) => c.endpoint.as_mut(),
            Self::Mcp(c) => c.endpoint.as_mut(),
            Self::Acp(c) => c.endpoint.as_mut(),
            Self::Search(c) => c.endpoint.as_mut(),
        }
    }
}

/// Result of probing a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionCheck {
    pub healthy: bool,
    pub checked_at: String,
    pub failure: Option<Failure>,
    pub capabilities: Vec<String>,
    pub resources: Vec<String>,
}

/// Saved host connection entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub credential_state: String,
    pub revision: u64,
    pub kind: ConnectionKind,
    pub last_check: Option<ConnectionCheck>,
    pub config: Option<ConnectionConfig>,
}

impl Connection {
    /// Returns the endpoint metadata if the config carries one.
    pub fn endpoint(&self) -> Option<&ConnectionEndpoint> {
        self.config.as_ref()?.endpoint()
    }

    /// Returns the endpoint metadata if this is an LLM connection.
    pub fn llm_endpoint(&self) -> Option<&ConnectionEndpoint> {
        match self.config.as_ref()? {
            ConnectionConfig::Llm(llm) => llm.endpoint.as_ref(),
            _ => None,
        }
    }

    /// Returns true if this connection targets Sakura AI Engine.
    pub fn is_sakura(&self) -> bool {
        self.endpoint()
            .is_some_and(|ep| ene_catalog::is_sakura_connection(&ep.adapter, &ep.endpoint))
    }

    /// Returns true if this connection targets a local VOICEVOX TTS engine.
    pub fn is_local_voicevox(&self) -> bool {
        self.kind == ConnectionKind::Tts
            && !self.is_sakura()
            && matches!(
                &self.config,
                Some(ConnectionConfig::Tts(tts))
                    if tts
                        .endpoint
                        .as_ref()
                        .is_some_and(|ep| ep.adapter.eq_ignore_ascii_case("voicevox"))
            )
    }

    /// Returns the trimmed search adapter name if this is a search connection.
    pub fn search_adapter(&self) -> Option<&str> {
        if self.kind != ConnectionKind::Search {
            return None;
        }
        match self.config.as_ref()? {
            ConnectionConfig::Search(search) => search
                .endpoint
                .as_ref()
                .map(|ep| ep.adapter.trim())
                .filter(|adapter| !adapter.is_empty()),
            _ => None,
        }
    }

    /// Returns true when the connection requires an unregistered credential.
    pub fn needs_key(&self, providers: &[LlmProvider]) -> bool {
        if matches!(self.credential_state.as_str(), "registered" | "environment") {
            return false;
        }
        if self
            .search_adapter()
            .is_some_and(ene_catalog::search_adapter_requires_key)
        {
            return true;
        }
        self.llm_endpoint().is_some_and(|ep| {
            providers.iter().any(|provider| {
                provider.requires_credentials
                    && provider.adapter == ep.adapter
                    && (ep.endpoint.trim().is_empty()
                        || provider.endpoint.trim_end_matches('/')
                            == ep.endpoint.trim().trim_end_matches('/'))
            })
        })
    }

    /// Returns true if this LLM connection is ready to serve chat turns.
    pub fn can_start_chat(&self, providers: &[LlmProvider]) -> bool {
        self.enabled
            && self.kind == ConnectionKind::Llm
            && !self.needs_key(providers)
            && self.last_check.as_ref().is_none_or(|check| check.healthy)
    }

    /// Converts this saved connection into a mutable update payload.
    pub fn to_input(&self) -> ConnectionInput {
        ConnectionInput {
            id: self.id.clone(),
            name: self.name.clone(),
            enabled: self.enabled,
            expected_revision: self.revision,
            kind: self.kind,
            config: self.config.clone(),
        }
    }
}

/// Connection creation/update payload passed to settings mutations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInput {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub expected_revision: u64,
    pub kind: ConnectionKind,
    pub config: Option<ConnectionConfig>,
}

/// Provider setting schema field descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub choices: Vec<String>,
}

/// Host-advertised LLM provider catalog item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmProvider {
    pub id: String,
    pub name: String,
    pub adapter: String,
    pub endpoint: String,
    pub settings: Vec<SettingField>,
    pub requires_credentials: bool,
    pub environment_variable: Option<String>,
}

/// Model metadata entry returned by a connection's model listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

/// Installed host Wasm plugin descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub extensions: Vec<String>,
    pub path: String,
    pub installed_at: String,
}
