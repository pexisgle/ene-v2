//! Pure connection, provider, model, and plugin domain types.

use super::Failure;
use std::collections::BTreeMap;

/// Kind of host-managed external connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionKind {
    /// The host did not report a kind.
    #[default]
    Unspecified,
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
    /// Adapter name that selects the transport, such as `openai`.
    pub adapter: String,
    /// Base URL, empty when the adapter derives one.
    pub endpoint: String,
    /// JSON Schema object text for `options`.
    pub options_schema: String,
    /// JSON object text conforming to `options_schema`.
    pub options: String,
}

/// LLM connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Model ids this endpoint serves.
    pub model_ids: Vec<String>,
}

/// Text-to-speech connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TtsConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Voice ids this endpoint can synthesise.
    pub voice_ids: Vec<String>,
}

/// Speech-to-text connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SttConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Language hints for recognition; empty lets the service detect.
    pub language_hints: Vec<String>,
}

/// Text embedding connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbeddingConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Embedding model to call.
    pub model_id: String,
    /// Width of the returned vector, checked against the model.
    pub dimensions: u32,
}

/// Model Context Protocol (MCP) server connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Tools the host may call; empty allows every tool the server lists.
    pub allowed_tools: Vec<String>,
    /// Executable launched for a stdio server; empty for a remote one.
    pub command: String,
    /// Arguments passed to `command`, never a shell string.
    pub args: Vec<String>,
    /// Working directory for `command`; empty inherits the host's.
    pub cwd: String,
    /// Environment variables added to `command`.
    pub env: BTreeMap<String, String>,
    /// HTTP headers sent to a remote server.
    pub headers: BTreeMap<String, String>,
    /// Header name the stored secret is sent in.
    pub auth_header: String,
    /// Environment variable holding the secret, read at connect time.
    pub secret_env_var: String,
}

/// Agent Client Protocol (ACP) harness connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Executable launched for the agent.
    pub command: String,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
}

/// Web search provider connection settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchConnection {
    /// The shared adapter, URL and options block.
    pub endpoint: Option<ConnectionEndpoint>,
    /// Result cap per query, at most 20.
    pub max_results: u32,
}

/// Kind-specific connection configuration payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionConfig {
    /// Chat and other text generation.
    Llm(LlmConnection),
    /// Text to speech.
    Tts(TtsConnection),
    /// Speech to text.
    Stt(SttConnection),
    /// Vector embeddings.
    Embedding(EmbeddingConnection),
    /// A Model Context Protocol server.
    Mcp(McpConnection),
    /// An external Agent Client Protocol agent.
    Acp(AcpConnection),
    /// A web search backend.
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
    /// Whether the probe reached the service and passed.
    pub healthy: bool,
    /// When the probe ran, as the host stamped it.
    pub checked_at: String,
    /// Why the probe failed, when it did.
    pub failure: Option<Failure>,
    /// Capabilities the service advertised, such as streaming or tool calls.
    pub capabilities: Vec<String>,
    /// Resources the service listed, such as models or voices.
    pub resources: Vec<String>,
}

/// Saved host connection entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    /// Id of this connection.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Whether the host may use the connection.
    pub enabled: bool,
    /// Whether a secret is stored, in the host's own spelling.
    pub credential_state: String,
    /// Optimistic-concurrency counter, bumped on every write.
    pub revision: u64,
    /// Which kind of service this connection reaches.
    pub kind: ConnectionKind,
    /// Result of the most recent probe, when one has run.
    pub last_check: Option<ConnectionCheck>,
    /// Adapter, URL and kind-specific fields. Must match `kind`.
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
    /// Id to create or update.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Whether the host may use the connection.
    pub enabled: bool,
    /// The revision the edit was based on, so a stale write is refused.
    pub expected_revision: u64,
    /// Which kind of service the connection reaches.
    pub kind: ConnectionKind,
    /// Adapter, URL and kind-specific fields.
    pub config: Option<ConnectionConfig>,
}

/// Provider setting schema field descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingField {
    /// Key the field's value is stored under.
    pub key: String,
    /// Label the host shows for the field.
    pub label: String,
    /// Input kind the host renders, such as `text` or `select`.
    pub kind: String,
    /// Whether the field must be filled in.
    pub required: bool,
    /// Allowed values, for a `select` field.
    pub choices: Vec<String>,
}

/// Host-advertised LLM provider catalog item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmProvider {
    /// Provider id, unique in the catalog.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Adapter name the driver selects on.
    pub adapter: String,
    /// Default base URL for the provider.
    pub endpoint: String,
    /// Configuration fields the host renders for this provider.
    pub settings: Vec<SettingField>,
    /// Whether the provider needs a credential before it can be used.
    pub requires_credentials: bool,
    /// Environment variable the credential may be read from instead.
    pub environment_variable: Option<String>,
}

/// Model metadata entry returned by a connection's model listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model id passed to the provider.
    pub id: String,
    /// Model name shown to the operator.
    pub name: String,
}

/// Installed host Wasm plugin descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginInfo {
    /// Package id, unique among installed plugins.
    pub id: String,
    /// Display name shown to the operator.
    pub name: String,
    /// Release version of the plugin.
    pub version: String,
    /// Operator-facing summary.
    pub description: String,
    /// Whether the host may run the plugin.
    pub enabled: bool,
    /// Extensions the plugin provides, in the host's own spelling.
    pub extensions: Vec<String>,
    /// Where the verified plugin snapshot lives on disk.
    pub path: String,
    /// When the plugin was installed, as the host stamped it.
    pub installed_at: String,
}
