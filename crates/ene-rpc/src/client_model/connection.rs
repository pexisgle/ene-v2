//! Wire conversions for connection, provider, model, and plugin types.

use crate as w;
use ene_domain::{ConnectionId, DomainError, client_view as domain, settings};

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

// Clients can display absent endpoints. Persisted connections always carry
// one, and a write preserves the legacy interpretation of an omitted endpoint.
macro_rules! connection_configs {
    ($($variant:ident($config:ident) {
        fields: [$($field:ident),*],
        scalars: [$($scalar:ident),*],
        maps: [$($map:ident),*]
    }),+ $(,)?) => {
        fn decode_config<E>(
            value: w::connection_config::Config,
            endpoint: impl FnOnce(Option<settings::Endpoint>) -> E,
        ) -> settings::ConnectionConfig<E> {
            match value {
                $(w::connection_config::Config::$variant(config) => settings::ConnectionConfig::$variant(settings::$config {
                    endpoint: endpoint(config.endpoint.into_option().map(Into::into)),
                    $($field: config.$field,)*
                    $($scalar: config.$scalar,)*
                    $($map: config.$map.into_iter().collect(),)*
                }),)+
            }
        }

        impl From<w::connection_config::Config> for domain::ConnectionConfig {
            fn from(value: w::connection_config::Config) -> Self {
                decode_config(value, std::convert::identity)
            }
        }

        impl From<domain::ConnectionConfig> for w::ConnectionConfig {
            fn from(value: domain::ConnectionConfig) -> Self {
                let config = match value {
                    $(settings::ConnectionConfig::$variant(config) => w::connection_config::Config::$variant(Box::new(w::$config {
                        endpoint: config.endpoint.map(Into::into).into(),
                        $($field: config.$field,)*
                        $($scalar: config.$scalar,)*
                        $($map: config.$map.into_iter().collect(),)*
                        ..Default::default()
                    })),)+
                };
                Self { config: Some(config), ..Default::default() }
            }
        }

        impl From<&settings::ConnectionConfig> for w::ConnectionConfig {
            fn from(value: &settings::ConnectionConfig) -> Self {
                let config = match value {
                    $(settings::ConnectionConfig::$variant(config) => w::connection_config::Config::$variant(Box::new(w::$config {
                        endpoint: Some((&config.endpoint).into()).into(),
                        $($field: config.$field.clone(),)*
                        $($scalar: config.$scalar,)*
                        $($map: config.$map.iter().map(|(key, value)| (key.clone(), value.clone())).collect(),)*
                        ..Default::default()
                    })),)+
                };
                Self { config: Some(config), ..Default::default() }
            }
        }
    };
}

connection_configs! {
    Llm(LlmConnection) { fields: [model_ids], scalars: [], maps: [] },
    Tts(TtsConnection) { fields: [voice_ids], scalars: [], maps: [] },
    Stt(SttConnection) { fields: [language_hints], scalars: [], maps: [] },
    Embedding(EmbeddingConnection) { fields: [model_id], scalars: [dimensions], maps: [] },
    Mcp(McpConnection) {
        fields: [allowed_tools, command, args, cwd, auth_header, secret_env_var],
        scalars: [],
        maps: [env, headers]
    },
    Acp(AcpConnection) { fields: [command, args], scalars: [], maps: [] },
    Search(SearchConnection) { fields: [], scalars: [max_results], maps: [] },
}

impl TryFrom<w::ConnectionInput> for settings::Connection {
    type Error = DomainError;

    fn try_from(input: w::ConnectionInput) -> Result<Self, Self::Error> {
        let kind = input
            .kind
            .as_known()
            .unwrap_or(w::ConnectionKind::Unspecified)
            .try_into()?;
        let config = input
            .config
            .into_option()
            .and_then(|config| config.config)
            .ok_or_else(|| DomainError::Invalid("connection config is required".into()))?;
        Ok(Self {
            id: ConnectionId::try_from(input.id)?,
            name: input.name,
            enabled: input.enabled,
            kind,
            config: decode_config(config, Option::unwrap_or_default),
            credential_state: settings::CredentialState::Absent,
            revision: 0,
            last_check: None,
        })
    }
}

impl From<&settings::Connection> for w::Connection {
    fn from(value: &settings::Connection) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            name: value.name.clone(),
            enabled: value.enabled,
            credential_state: value.credential_state.as_str().into(),
            revision: value.revision,
            kind: w::ConnectionKind::from(value.kind).into(),
            last_check: value.last_check.as_ref().map(Into::into).into(),
            config: Some((&value.config).into()).into(),
            ..Default::default()
        }
    }
}

impl From<&settings::Settings> for w::Settings {
    fn from(value: &settings::Settings) -> Self {
        Self {
            revision: value.revision,
            connections: value.connections.iter().map(Into::into).collect(),
            updated_at: value.updated_at.clone(),
            policy_texts: value.policy_texts.iter().map(Into::into).collect(),
            ..Default::default()
        }
    }
}

from_fields! {
    w::Connection => domain::Connection {
        id, name, enabled, credential_state.parse().unwrap_or_default(), revision,
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
    &w::LlmProviderView<'_> => domain::LlmProvider {
        id.to_owned(), name.to_owned(), adapter.to_owned(), endpoint.to_owned(),
        requires_credentials,
    }
}

impl From<&w::ModelView<'_>> for domain::ModelInfo {
    fn from(value: &w::ModelView<'_>) -> Self {
        Self {
            id: value.id.to_owned(),
            name: value.name.to_owned(),
        }
    }
}

from_fields! {
    &w::PluginInfoView<'_> => domain::PluginInfo {
        id.to_owned(), name.to_owned(), version.to_owned(), enabled,
        extensions.iter().copied().map(str::to_owned).collect(),
    }
}
