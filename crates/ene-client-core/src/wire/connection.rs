//! Wire conversions for connection, provider, model, and plugin types.

use crate::domain;
use ene_rpc as w;

impl From<w::ConnectionKind> for domain::ConnectionKind {
    fn from(value: w::ConnectionKind) -> Self {
        match value {
            w::ConnectionKind::Unspecified => Self::Unspecified,
            w::ConnectionKind::Llm => Self::Llm,
            w::ConnectionKind::Tts => Self::Tts,
            w::ConnectionKind::Stt => Self::Stt,
            w::ConnectionKind::Embedding => Self::Embedding,
            w::ConnectionKind::Mcp => Self::Mcp,
            w::ConnectionKind::Acp => Self::Acp,
            w::ConnectionKind::Search => Self::Search,
        }
    }
}

impl From<domain::ConnectionKind> for w::ConnectionKind {
    fn from(value: domain::ConnectionKind) -> Self {
        match value {
            domain::ConnectionKind::Unspecified => Self::Unspecified,
            domain::ConnectionKind::Llm => Self::Llm,
            domain::ConnectionKind::Tts => Self::Tts,
            domain::ConnectionKind::Stt => Self::Stt,
            domain::ConnectionKind::Embedding => Self::Embedding,
            domain::ConnectionKind::Mcp => Self::Mcp,
            domain::ConnectionKind::Acp => Self::Acp,
            domain::ConnectionKind::Search => Self::Search,
        }
    }
}

impl From<w::ConnectionEndpoint> for domain::ConnectionEndpoint {
    fn from(value: w::ConnectionEndpoint) -> Self {
        Self {
            adapter: value.adapter,
            endpoint: value.endpoint,
            options_schema: value.options_schema,
            options: value.options,
        }
    }
}

impl From<domain::ConnectionEndpoint> for w::ConnectionEndpoint {
    fn from(value: domain::ConnectionEndpoint) -> Self {
        Self {
            adapter: value.adapter,
            endpoint: value.endpoint,
            options_schema: value.options_schema,
            options: value.options,
            ..Default::default()
        }
    }
}

impl From<w::connection_config::Config> for domain::ConnectionConfig {
    fn from(cfg: w::connection_config::Config) -> Self {
        match cfg {
            w::connection_config::Config::Llm(c) => {
                domain::ConnectionConfig::Llm(domain::LlmConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    model_ids: c.model_ids,
                })
            }
            w::connection_config::Config::Tts(c) => {
                domain::ConnectionConfig::Tts(domain::TtsConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    voice_ids: c.voice_ids,
                })
            }
            w::connection_config::Config::Stt(c) => {
                domain::ConnectionConfig::Stt(domain::SttConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    language_hints: c.language_hints,
                })
            }
            w::connection_config::Config::Embedding(c) => {
                domain::ConnectionConfig::Embedding(domain::EmbeddingConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    model_id: c.model_id,
                    dimensions: c.dimensions,
                })
            }
            w::connection_config::Config::Mcp(c) => {
                domain::ConnectionConfig::Mcp(domain::McpConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    allowed_tools: c.allowed_tools,
                    command: c.command,
                    args: c.args,
                    cwd: c.cwd,
                    env: c.env.into_iter().collect(),
                    headers: c.headers.into_iter().collect(),
                    auth_header: c.auth_header,
                    secret_env_var: c.secret_env_var,
                })
            }
            w::connection_config::Config::Acp(c) => {
                domain::ConnectionConfig::Acp(domain::AcpConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    command: c.command,
                    args: c.args,
                })
            }
            w::connection_config::Config::Search(c) => {
                domain::ConnectionConfig::Search(domain::SearchConnection {
                    endpoint: c.endpoint.into_option().map(Into::into),
                    max_results: c.max_results,
                })
            }
        }
    }
}

impl From<domain::ConnectionConfig> for w::ConnectionConfig {
    fn from(value: domain::ConnectionConfig) -> Self {
        let config = match value {
            domain::ConnectionConfig::Llm(c) => {
                w::connection_config::Config::Llm(Box::new(w::LlmConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    model_ids: c.model_ids,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Tts(c) => {
                w::connection_config::Config::Tts(Box::new(w::TtsConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    voice_ids: c.voice_ids,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Stt(c) => {
                w::connection_config::Config::Stt(Box::new(w::SttConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    language_hints: c.language_hints,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Embedding(c) => {
                w::connection_config::Config::Embedding(Box::new(w::EmbeddingConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    model_id: c.model_id,
                    dimensions: c.dimensions,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Mcp(c) => {
                w::connection_config::Config::Mcp(Box::new(w::McpConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    allowed_tools: c.allowed_tools,
                    command: c.command,
                    args: c.args,
                    cwd: c.cwd,
                    env: c.env.into_iter().collect(),
                    headers: c.headers.into_iter().collect(),
                    auth_header: c.auth_header,
                    secret_env_var: c.secret_env_var,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Acp(c) => {
                w::connection_config::Config::Acp(Box::new(w::AcpConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    command: c.command,
                    args: c.args,
                    ..Default::default()
                }))
            }
            domain::ConnectionConfig::Search(c) => {
                w::connection_config::Config::Search(Box::new(w::SearchConnection {
                    endpoint: c.endpoint.map(Into::into).into(),
                    max_results: c.max_results,
                    ..Default::default()
                }))
            }
        };
        Self {
            config: Some(config),
            ..Default::default()
        }
    }
}

impl From<w::ConnectionCheck> for domain::ConnectionCheck {
    fn from(value: w::ConnectionCheck) -> Self {
        Self {
            healthy: value.healthy,
            checked_at: value.checked_at,
            failure: value.failure.into_option().map(Into::into),
            capabilities: value.capabilities,
            resources: value.resources,
        }
    }
}

impl From<w::Connection> for domain::Connection {
    fn from(value: w::Connection) -> Self {
        Self {
            id: value.id,
            name: value.name,
            enabled: value.enabled,
            credential_state: value.credential_state,
            revision: value.revision,
            kind: value.kind.as_known().map(Into::into).unwrap_or_default(),
            last_check: value.last_check.into_option().map(Into::into),
            config: value
                .config
                .into_option()
                .and_then(|cfg| cfg.config.map(Into::into)),
        }
    }
}

impl From<domain::ConnectionInput> for w::ConnectionInput {
    fn from(value: domain::ConnectionInput) -> Self {
        Self {
            id: value.id,
            name: value.name,
            enabled: value.enabled,
            expected_revision: value.expected_revision,
            kind: w::ConnectionKind::from(value.kind).into(),
            config: value.config.map(Into::into).into(),
            ..Default::default()
        }
    }
}

impl From<w::SettingField> for domain::SettingField {
    fn from(value: w::SettingField) -> Self {
        Self {
            key: value.key,
            label: value.label,
            kind: value.kind,
            required: value.required,
            choices: value.choices,
        }
    }
}

impl From<w::LlmProvider> for domain::LlmProvider {
    fn from(value: w::LlmProvider) -> Self {
        Self {
            id: value.id,
            name: value.name,
            adapter: value.adapter,
            endpoint: value.endpoint,
            settings: value.settings.into_iter().map(Into::into).collect(),
            requires_credentials: value.requires_credentials,
            environment_variable: value.environment_variable,
        }
    }
}

impl From<w::Model> for domain::ModelInfo {
    fn from(value: w::Model) -> Self {
        Self {
            id: value.id,
            name: value.name,
        }
    }
}

impl From<w::PluginInfo> for domain::PluginInfo {
    fn from(value: w::PluginInfo) -> Self {
        Self {
            id: value.id,
            name: value.name,
            version: value.version,
            description: value.description,
            enabled: value.enabled,
            extensions: value.extensions,
            path: value.path,
            installed_at: value.installed_at,
        }
    }
}
