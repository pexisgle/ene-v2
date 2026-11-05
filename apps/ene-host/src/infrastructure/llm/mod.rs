mod openai;
mod openrouter;
pub(crate) mod plugin;
pub mod registry;

/// The parent conversation's model handler. The built-in providers use Rig's
/// native wires through the host's authorized transport. A plugin adapter
/// speaks its own loop and is refused here rather than silently treated as
/// one of them.
pub(crate) fn agent_connection(
    outbound: std::sync::Arc<crate::llm::Outbound>,
    model: &str,
) -> crate::ports::Result<rig_ecs::model::Connection> {
    match outbound.connection().adapter.as_str() {
        openai::ID => openai::agent_connection(outbound, model),
        openrouter::ID => openrouter::agent_connection(outbound, model),
        _ => Err(crate::domain::Error {
            kind: crate::domain::ErrorKind::UnsupportedCapability,
            reason: Some("adapter_unsupported"),
        }),
    }
}

pub(crate) fn can_serve_conversation(connection: &crate::domain::Connection) -> bool {
    connection.kind == "llm" && matches!(connection.adapter.as_str(), openai::ID | openrouter::ID)
}

/// The fields every builtin descriptor shares. Adapters override
/// `destinations`, `models` and `capabilities`.
fn builtin_descriptor(
    id: &str,
    name: &str,
    endpoint: &str,
    environment_variable: &str,
    revision: &str,
) -> crate::llm::Descriptor {
    crate::llm::Descriptor {
        id: id.into(),
        name: name.into(),
        kind: "builtin".into(),
        endpoint: endpoint.into(),
        options: ene_rpc::JsonDocument::from(serde_json::json!({})).into(),
        requires_credentials: true,
        environment_variable: Some(environment_variable.into()),
        state: "ready".into(),
        revision: revision.into(),
        ..Default::default()
    }
}

/// The adapter identity, endpoint and explicit tool-calling declaration a
/// provider policy must match. Only the destinations differ per adapter.
fn compatible_policy(
    connection: &crate::domain::Connection,
    id: &str,
    endpoint: &str,
    revision: &str,
    environment_variable: &str,
    destinations: Vec<String>,
) -> crate::ports::Result<crate::llm::Policy> {
    if connection.kind != "llm"
        || connection.adapter != id
        || connection.endpoint != endpoint
        || explicit_tool_calling(connection).is_err()
    {
        return Err(crate::domain::Error::new(
            crate::domain::ErrorKind::UnsupportedCapability,
        ));
    }
    Ok(crate::llm::Policy {
        revision: revision.into(),
        fingerprint: serde_json::json!({"adapter": connection.adapter, "endpoint": connection.endpoint, "options": serde_json::from_str::<serde_json::Value>(&connection.options.json).unwrap_or(serde_json::Value::Null), "implementation": revision}),
        destinations,
        requires_credentials: true,
        environment_variable: Some(environment_variable.into()),
    })
}

/// One authorized `GET /models`; adapters only map the returned items.
async fn catalog_items(
    transport: crate::llm::AuthorizedTransport,
    endpoint: &str,
) -> crate::ports::Result<Vec<serde_json::Value>> {
    let request = http::Request::get(format!("{endpoint}/models"))
        .body(bytes::Bytes::new())
        .map_err(|_| crate::domain::Error::invalid())?;
    let response = transport.send(request).await?;
    if !response.status().is_success() {
        return Err(crate::domain::Error::new(
            crate::domain::ErrorKind::ProviderFailure,
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&response.bytes().await?)
        .map_err(|_| crate::domain::Error::new(crate::domain::ErrorKind::ExternalFailure))?;
    value["data"]
        .as_array()
        .cloned()
        .ok_or_else(|| crate::domain::Error::new(crate::domain::ErrorKind::ExternalFailure))
}

/// Resolves the parent conversation's catalog model and refuses one that cannot
/// call tools. Shared by the built-in adapters, which differ only in wire.
pub(crate) fn tool_spec(
    connection: &crate::domain::Connection,
    adapter: &str,
    model: &str,
) -> crate::ports::Result<(std::sync::Arc<rig_core::catalog::ModelSpec>, String)> {
    use rig_core::catalog::{Catalog, ModelSpec};
    use rig_core::providers::registry::ProviderId;
    let reference = format!("{adapter}/{model}");
    let spec = match Catalog::builtin().resolve(&reference) {
        Ok(resolved) => resolved.shared(),
        Err(_) => match explicit_tool_calling(connection)? {
            Some(true) => {
                let provider = ProviderId::catalog(adapter).ok_or_else(|| {
                    crate::domain::Error::new(crate::domain::ErrorKind::UnsupportedCapability)
                })?;
                std::sync::Arc::new(ModelSpec::new(provider, model).with_tools(true))
            }
            Some(false) => return Err(tool_error("model_cannot_call_tools")),
            None => return Err(tool_error("model_capability_unknown")),
        },
    };
    if !spec.tools {
        return Err(tool_error("model_cannot_call_tools"));
    }
    Ok((spec, reference))
}

fn tool_error(reason: &'static str) -> crate::domain::Error {
    crate::domain::Error {
        kind: crate::domain::ErrorKind::UnsupportedCapability,
        reason: Some(reason),
    }
}

/// An explicit declaration that the configured model calls tools, for a model
/// the Rig catalog does not list. `None` leaves the capability unknown.
pub(crate) fn explicit_tool_calling(
    options: &crate::domain::Connection,
) -> crate::ports::Result<Option<bool>> {
    let value: serde_json::Value =
        serde_json::from_str(&options.options.json).map_err(|_| crate::domain::Error::invalid())?;
    let Some(object) = value.as_object() else {
        return Err(crate::domain::Error::invalid());
    };
    let mut tool_calling = None;
    for (key, value) in object {
        if key != "tool_calling" {
            return Err(crate::domain::Error::invalid());
        }
        tool_calling = Some(value.as_bool().ok_or_else(crate::domain::Error::invalid)?);
    }
    Ok(tool_calling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::TestProvider;
    use crate::{
        domain::{Connection, OperationKind},
        llm::{Authorization, Outbound, Policy, ProviderBinding},
        ports::Result,
    };
    use futures_util::future::BoxFuture;
    use std::sync::Arc;
    struct Allow;
    impl Authorization for Allow {
        fn check(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn outbound(adapter: &str, endpoint: &str, options: &str) -> Arc<Outbound> {
        let connection = Connection {
            id: "c".into(),
            kind: "llm".into(),
            adapter: adapter.into(),
            endpoint: endpoint.into(),
            options: ene_rpc::JsonDocument {
                json: options.into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let origin = url::Url::parse(endpoint)
            .unwrap()
            .origin()
            .ascii_serialization();
        let policy = Policy {
            revision: "test".into(),
            fingerprint: serde_json::json!({}),
            destinations: vec![origin],
            requires_credentials: false,
            environment_variable: None,
        };
        let binding =
            ProviderBinding::resolve(Arc::new(TestProvider(policy)), &connection).unwrap();
        Arc::new(Outbound::new(
            connection,
            None,
            Arc::new(Allow),
            binding,
            OperationKind::Conversation,
        ))
    }

    /// A model outside the built-in catalog is refused as capability-unknown
    /// unless the connection declares tool calling, and a catalog model that
    /// does not call tools is refused as such.
    #[tokio::test]
    async fn an_unknown_model_needs_an_explicit_tool_declaration() {
        for (adapter, endpoint) in [
            ("openai", "https://api.openai.com/v1"),
            ("openrouter", "https://openrouter.ai/api/v1"),
        ] {
            let error = agent_connection(outbound(adapter, endpoint, "{}"), "unknown/not-listed")
                .err()
                .expect("unknown model without a declaration");
            assert_eq!(
                error.reason,
                Some("model_capability_unknown"),
                "{adapter}: {error:?}"
            );
            agent_connection(
                outbound(adapter, endpoint, r#"{"tool_calling": true}"#),
                "unknown/not-listed",
            )
            .expect("an explicit tool declaration is enough for an unknown model");
            assert!(
                agent_connection(
                    outbound(adapter, endpoint, r#"{"tool_calling": false}"#),
                    "unknown/not-listed",
                )
                .is_err(),
                "an explicit tool refusal is refused"
            );
        }
        // The catalog still says this model does not call tools, even when
        // the connection declares otherwise.
        let error = agent_connection(
            outbound(
                "openrouter",
                "https://openrouter.ai/api/v1",
                r#"{"tool_calling": true}"#,
            ),
            "cohere/command-a",
        )
        .err()
        .expect("a known no-tools model");
        assert_eq!(error.reason, Some("model_cannot_call_tools"));
    }
}
