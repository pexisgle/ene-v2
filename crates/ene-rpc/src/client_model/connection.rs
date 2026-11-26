//! Wire conversions for connection, provider, model, and plugin types.

use crate as w;
use ene_domain::client_view as domain;

from_variants! {
    w::ConnectionKind => domain::ConnectionKind [
        Unspecified, Llm, Tts, Stt, Embedding, Mcp, Acp, Search,
    ]
}

from_variants! {
    domain::ConnectionKind => w::ConnectionKind [
        Unspecified, Llm, Tts, Stt, Embedding, Mcp, Acp, Search,
    ]
}

// Both directions share the variants and their fields. Only the optional
// endpoint and protobuf wrapper have a different representation.
macro_rules! connection_configs {
    ($($variant:ident($config:ident) { $($field:ident $(.$method:ident())*),+ $(,)? }),+ $(,)?) => {
        impl From<w::connection_config::Config> for domain::ConnectionConfig {
            fn from(value: w::connection_config::Config) -> Self {
                match value {
                    $(w::connection_config::Config::$variant(config) => Self::$variant(domain::$config {
                        endpoint: config.endpoint.into_option().map(Into::into),
                        $($field: config.$field $(.$method())*,)+
                    }),)+
                }
            }
        }

        impl From<domain::ConnectionConfig> for w::ConnectionConfig {
            fn from(value: domain::ConnectionConfig) -> Self {
                let config = match value {
                    $(domain::ConnectionConfig::$variant(config) => w::connection_config::Config::$variant(
                        Box::new(w::$config {
                            endpoint: config.endpoint.map(Into::into).into(),
                            $($field: config.$field $(.$method())*,)+
                            ..Default::default()
                        })
                    ),)+
                };
                Self { config: Some(config), ..Default::default() }
            }
        }
    };
}

connection_configs! {
    Llm(LlmConnection) { model_ids },
    Tts(TtsConnection) { voice_ids },
    Stt(SttConnection) { language_hints },
    Embedding(EmbeddingConnection) { model_id, dimensions },
    Mcp(McpConnection) {
        allowed_tools, command, args, cwd, auth_header, secret_env_var,
        env.into_iter().collect(),
        headers.into_iter().collect(),
    },
    Acp(AcpConnection) { command, args },
    Search(SearchConnection) { max_results },
}

from_fields! {
    w::Connection => domain::Connection {
        id, name, enabled, credential_state, revision,
        kind.as_known().map_or_default(Into::into),
        last_check.into_option().map(Into::into),
        config.into_option().and_then(|cfg| cfg.config.map(Into::into)),
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

from_fields! {
    w::SettingField => domain::SettingField {
        key, label, kind, required, choices,
    }
}

from_fields! {
    w::LlmProvider => domain::LlmProvider {
        id, name, adapter, endpoint,
        settings.into_iter().map(Into::into).collect(),
        requires_credentials, environment_variable,
    }
}

from_fields! {
    w::Model => domain::ModelInfo {
        id, name,
    }
}

from_fields! {
    w::PluginInfo => domain::PluginInfo {
        id, name, version, description, enabled, extensions, path, installed_at,
    }
}
