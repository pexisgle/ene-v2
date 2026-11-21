//! The host services a provider plugin may call.
//!
//! A provider reaches its endpoint through the host's broker, never directly,
//! so the credential stays here and every transport guard applies. This is the
//! one implementation of that boundary: the speech and completion adapters
//! both hand it to their instance.
use ene_plugin::{Failure, ServiceRequest};
use ene_plugin_runtime::{
    instance::Services,
    service::{HttpBroker, HttpGrants},
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn http_broker(grants: Arc<dyn HttpGrants>) -> Result<Arc<HttpBroker>, reqwest::Error> {
    broker(grants, None)
}

/// A broker whose exchange may take `deadline`, for a path that legitimately
/// streams longer than the transport default.
pub(crate) fn http_broker_with_deadline(
    grants: Arc<dyn HttpGrants>,
    deadline: std::time::Duration,
) -> Result<Arc<HttpBroker>, reqwest::Error> {
    broker(grants, Some(deadline))
}

fn broker(
    grants: Arc<dyn HttpGrants>,
    deadline: Option<std::time::Duration>,
) -> Result<Arc<HttpBroker>, reqwest::Error> {
    #[cfg(test)]
    if construction_failure::active() {
        // A real builder error without depending on TLS or process-wide state.
        reqwest::Client::builder().user_agent("\n").build()?;
    }
    match deadline {
        Some(deadline) => HttpBroker::with_deadline(grants, deadline).map(Arc::new),
        None => HttpBroker::new(grants).map(Arc::new),
    }
}

/// The services a provider call declares: the broker, and nothing else.
pub(crate) struct BrokerServices {
    broker: Arc<HttpBroker>,
    provider: String,
}

impl BrokerServices {
    /// Serves `provider`'s exchanges through `broker`.
    pub(crate) fn new(broker: Arc<HttpBroker>, provider: String) -> Arc<Self> {
        Arc::new(Self { broker, provider })
    }
}

/// Callback invoked when a plugin calls `ServiceRequest::Tool` (always gated
/// by the caller; fail-closed when absent).
pub(crate) type PluginToolCallback =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, Failure>> + Send + Sync>;

/// Full host-service implementation for non-provider and extension plugins
/// (`Log`, `Configuration`, isolated `State`, optional `HttpBroker`, and
/// optional policy-gated `Tool` invocation) (`F19`).
pub(crate) struct PluginServices {
    plugin_id: String,
    store: crate::host::db::Store,
    broker: Option<(Arc<HttpBroker>, String)>,
    tool_callback: Option<PluginToolCallback>,
}

impl PluginServices {
    pub(crate) fn new(plugin_id: impl Into<String>, store: crate::host::db::Store) -> Arc<Self> {
        Arc::new(Self {
            plugin_id: plugin_id.into(),
            store,
            broker: None,
            tool_callback: None,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_tool_callback(
        plugin_id: impl Into<String>,
        store: crate::host::db::Store,
        tool_callback: PluginToolCallback,
    ) -> Arc<Self> {
        Arc::new(Self {
            plugin_id: plugin_id.into(),
            store,
            broker: None,
            tool_callback: Some(tool_callback),
        })
    }
}

impl Services for PluginServices {
    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        let plugin_id = self.plugin_id.clone();
        let store = self.store.clone();
        let broker = self.broker.clone();
        let tool_callback = self.tool_callback.clone();
        Box::pin(async move {
            match request {
                ServiceRequest::Log { message } => {
                    let clipped = crate::host::util::truncate(&message, 4_096);
                    tracing::info!(plugin = %plugin_id, "{clipped}");
                    Ok(Value::Null)
                }
                ServiceRequest::Configuration => store
                    .plugin_config(&plugin_id)
                    .map_err(|_| Failure::new("service_failed")),
                ServiceRequest::StateRead { key } => {
                    let val = store
                        .plugin_state_read(&plugin_id, &key)
                        .map_err(|_| Failure::new("service_failed"))?;
                    Ok(val.unwrap_or(Value::Null))
                }
                ServiceRequest::StateWrite { key, value } => {
                    store
                        .plugin_state_write(&plugin_id, &key, &value)
                        .map_err(|_| Failure::new("service_failed"))?;
                    Ok(Value::Null)
                }
                ServiceRequest::HttpOpen { request } => {
                    let Some((broker, provider)) = broker else {
                        return Err(Failure::new("undeclared_service"));
                    };
                    serde_json::to_value(broker.open(&provider, request).await?)
                        .map_err(|_| Failure::new("serialize"))
                }
                ServiceRequest::HttpRead { handle } => {
                    let Some((broker, _)) = broker else {
                        return Err(Failure::new("undeclared_service"));
                    };
                    serde_json::to_value(broker.read(&handle).await?)
                        .map_err(|_| Failure::new("serialize"))
                }
                ServiceRequest::HttpClose { handle } => {
                    let Some((broker, _)) = broker else {
                        return Err(Failure::new("undeclared_service"));
                    };
                    broker.close(&handle)?;
                    Ok(Value::Null)
                }
                ServiceRequest::Tool { name, arguments } => {
                    let Some(cb) = tool_callback else {
                        return Err(Failure::new("permission_required"));
                    };
                    cb(name, arguments).await
                }
                ServiceRequest::AudioRead { .. } => Err(Failure::new("undeclared_service")),
            }
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

/// Resolved per-connection invocation context shared across LLM, embedding,
/// and speech provider plugins.
#[derive(Clone)]
pub(crate) struct ProviderInvocationContext {
    pub(crate) provider_id: String,
    pub(crate) endpoint: String,
    pub(crate) options: Value,
    pub(crate) broker: Arc<HttpBroker>,
}

impl ProviderInvocationContext {
    /// Builds an invocation context for one provider plugin and connection,
    /// constructing its HTTP broker from `Grant`.
    pub(crate) fn from_connection(
        running: &super::Running,
        connection: &crate::host::settings::Connection,
        credentials: Option<&crate::host::model::Credentials>,
        providers: &super::Providers,
        deadline: Option<std::time::Duration>,
    ) -> crate::host::error::Result<Self> {
        let grant = super::Grant::new(running.provider(), connection, credentials, providers)?;
        let broker = match deadline {
            Some(deadline) => http_broker_with_deadline(Arc::new(grant), deadline),
            None => http_broker(Arc::new(grant)),
        }
        .map_err(|error| {
            crate::host::error::HostError::Unavailable(format!("plugin HTTP transport: {error}"))
        })?;
        Ok(Self::with_broker(running, connection, providers, broker))
    }

    /// Builds an invocation context using a caller-provided broker.
    pub(crate) fn with_broker(
        running: &super::Running,
        connection: &crate::host::settings::Connection,
        providers: &super::Providers,
        broker: Arc<HttpBroker>,
    ) -> Self {
        let endpoint = crate::host::settings::endpoint_of(connection, providers)
            .unwrap_or_else(|| running.provider().endpoint.clone());
        let options = merge_options(&running.provider().options, &connection.endpoint().options);
        Self {
            provider_id: running.provider().id.clone(),
            endpoint,
            options,
            broker,
        }
    }

    /// Services handle backed by this context's broker and provider identity.
    pub(crate) fn services(&self) -> Arc<dyn Services> {
        BrokerServices::new(Arc::clone(&self.broker), self.provider_id.clone())
    }
}

/// Merges connection-level JSON options onto plugin manifest defaults.
pub(crate) fn merge_options(defaults: &Value, raw_connection_options: &str) -> Value {
    let trimmed = raw_connection_options.trim();
    let conn_val: Option<Value> = if trimmed.is_empty() {
        None
    } else {
        serde_json::from_str(trimmed).ok()
    };
    match (defaults, conn_val) {
        (Value::Object(base), Some(Value::Object(override_map))) => {
            let mut merged = base.clone();
            for (key, value) in override_map {
                merged.insert(key, value);
            }
            Value::Object(merged)
        }
        (_, Some(override_val @ Value::Object(_))) => override_val,
        (Value::Object(base), _) => Value::Object(base.clone()),
        (other, _) if !other.is_null() => other.clone(),
        _ => Value::Object(Default::default()),
    }
}

impl Services for BrokerServices {
    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        let broker = Arc::clone(&self.broker);
        let provider = self.provider.clone();
        Box::pin(async move {
            Ok(match request {
                ServiceRequest::HttpOpen { request } => {
                    serde_json::to_value(broker.open(&provider, request).await?)
                        .map_err(|_| Failure::new("serialize"))?
                }
                ServiceRequest::HttpRead { handle } => {
                    serde_json::to_value(broker.read(&handle).await?)
                        .map_err(|_| Failure::new("serialize"))?
                }
                ServiceRequest::HttpClose { handle } => {
                    broker.close(&handle)?;
                    Value::Null
                }
                // Audio is read by the host from its own synthesis answer, not
                // by the guest, so a plugin never asks for it.
                _ => return Err(Failure::new("undeclared_service")),
            })
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
pub(crate) mod construction_failure {
    use std::{cell::Cell, marker::PhantomData, rc::Rc};

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn active() -> bool {
        ACTIVE.get()
    }

    pub(crate) struct Guard {
        previous: bool,
        _thread: PhantomData<Rc<()>>,
    }

    impl Guard {
        pub(crate) fn enter() -> Self {
            Self {
                previous: ACTIVE.replace(true),
                _thread: PhantomData,
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.set(self.previous);
        }
    }
}
