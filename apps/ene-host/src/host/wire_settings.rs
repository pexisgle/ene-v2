//! Settings and connection conversions, split out of `wire` when it outgrew 500
//! lines. These already reached the proto contract, so `wire` re-exports the
//! three the RPC layer calls and `host::wire::` paths stay valid.

use super::{
    error::HostError,
    settings::{
        AcpConnection, Connection, ConnectionConfig, ConnectionKind, CredentialState,
        EmbeddingConnection, LlmConnection, McpConnection, SearchConnection, Settings,
        SttConnection, TtsConnection,
    },
    wire::wire_enum,
};
use ene_rpc as w;

/// Maps the connection-config oneof in both directions from one variant list:
/// the domain and proto config structs share their names and their kind-specific
/// fields.
macro_rules! connection_configs {
    ($($variant:ident($config:ident) {
        fields: [$($field:ident),* $(,)?],
        maps: [$($map:ident),* $(,)?] $(,)?
    }),+ $(,)?) => {
        fn config_to_wire(
            value: &ConnectionConfig,
            endpoint: &w::ConnectionEndpoint,
        ) -> w::connection_config::Config {
            match value {
                $(ConnectionConfig::$variant(config) => {
                    w::connection_config::Config::$variant(Box::new(w::$config {
                        endpoint: Some(endpoint.clone()).into(),
                        $($field: config.$field.clone(),)*
                        $($map: config.$map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),)*
                        ..Default::default()
                    }))
                }),+
            }
        }

        fn config_from_wire(value: w::connection_config::Config) -> ConnectionConfig {
            match value {
                $(w::connection_config::Config::$variant(config) => {
                    ConnectionConfig::$variant($config {
                        endpoint: config.endpoint.as_option().map_or_default(Into::into),
                        $($field: config.$field,)*
                        $($map: config.$map.into_iter().collect(),)*
                    })
                }),+
            }
        }
    };
}

connection_configs! {
    Llm(LlmConnection) { fields: [model_ids], maps: [] },
    Tts(TtsConnection) { fields: [voice_ids], maps: [] },
    Stt(SttConnection) { fields: [language_hints], maps: [] },
    Embedding(EmbeddingConnection) { fields: [model_id, dimensions], maps: [] },
    Mcp(McpConnection) {
        fields: [allowed_tools, command, args, cwd, auth_header, secret_env_var],
        maps: [env, headers],
    },
    Acp(AcpConnection) { fields: [command, args], maps: [] },
    Search(SearchConnection) { fields: [max_results], maps: [] },
}

/// Rebuilds the common endpoint from the contract.
/// Rebuilds a connection from a settings write.
pub(crate) fn connection_from(input: w::ConnectionInput) -> Result<Connection, HostError> {
    let kind = match input.kind.as_known() {
        Some(w::ConnectionKind::Llm) => ConnectionKind::Llm,
        Some(w::ConnectionKind::Tts) => ConnectionKind::Tts,
        Some(w::ConnectionKind::Stt) => ConnectionKind::Stt,
        Some(w::ConnectionKind::Embedding) => ConnectionKind::Embedding,
        Some(w::ConnectionKind::Mcp) => ConnectionKind::Mcp,
        Some(w::ConnectionKind::Acp) => ConnectionKind::Acp,
        Some(w::ConnectionKind::Search) => ConnectionKind::Search,
        Some(w::ConnectionKind::Unspecified) | None => {
            return Err(HostError::Invalid(("connection kind is required").into()));
        }
    };
    let config = input
        .config
        .into_option()
        .and_then(|config| config.config)
        .map(config_from_wire)
        .ok_or_else(|| HostError::Invalid(("connection config is required").into()))?;
    Ok(Connection {
        id: input.id,
        name: input.name,
        enabled: input.enabled,
        kind,
        config,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    })
}

wire_enum!(connection_kind: ConnectionKind => w::ConnectionKind {
    Llm, Tts, Stt, Embedding, Mcp, Acp, Search
});

pub(crate) fn connection(value: &Connection) -> w::Connection {
    let endpoint = value.endpoint().into();
    let config = config_to_wire(&value.config, &endpoint);
    w::Connection {
        id: value.id.clone(),
        name: value.name.clone(),
        enabled: value.enabled,
        credential_state: value.credential_state.as_str().into(),
        revision: value.revision,
        kind: connection_kind(value.kind).into(),
        last_check: value.last_check.as_ref().map(Into::into).into(),
        config: Some(w::ConnectionConfig {
            config: Some(config),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

pub(crate) fn settings(value: &Settings) -> w::Settings {
    w::Settings {
        revision: value.revision,
        connections: value.connections.iter().map(connection).collect(),
        updated_at: value.updated_at.clone(),
        policy_texts: value.policy_texts.iter().map(Into::into).collect(),
        ..Default::default()
    }
}
