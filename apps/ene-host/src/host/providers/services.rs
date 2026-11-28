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
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(crate) fn http_broker(grants: Arc<dyn HttpGrants>) -> Result<Arc<HttpBroker>, reqwest::Error> {
    broker(grants, None, CancellationToken::new())
}

fn broker(
    grants: Arc<dyn HttpGrants>,
    deadline: Option<std::time::Duration>,
    lifetime: CancellationToken,
) -> Result<Arc<HttpBroker>, reqwest::Error> {
    #[cfg(test)]
    if construction_failure::active() {
        // A real builder error without depending on TLS or process-wide state.
        reqwest::Client::builder().user_agent("\n").build()?;
    }
    HttpBroker::with_lifetime(grants, deadline, lifetime).map(Arc::new)
}

/// The services a provider call declares: the broker, and nothing else.
pub(crate) struct BrokerServices {
    pub(crate) broker: HttpBroker,
    provider: Arc<str>,
}

/// Logging, configuration and isolated state for non-provider extensions.
pub(crate) struct PluginServices {
    running: Arc<super::Running>,
    store: crate::host::db::Store,
}

impl PluginServices {
    pub(crate) fn new(running: Arc<super::Running>, store: crate::host::db::Store) -> Arc<Self> {
        Arc::new(Self { running, store })
    }
}

impl Services for PluginServices {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async {
            if self.running.revoked() {
                return Err(Failure::new("provider_revoked"));
            }
            Ok(())
        })
    }

    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            let running = &self.running;
            let plugin_id = running.plugin_id();
            self.store
                .plugin_repo()
                .with_installation(
                    plugin_id,
                    running.installation_id(),
                    || !running.revoked(),
                    |data| match request {
                        ServiceRequest::Log { message } => {
                            let clipped = ene_store::util::truncate(&message, 4_096);
                            tracing::info!(plugin = %plugin_id, "{clipped}");
                            Ok(Value::Null)
                        }
                        ServiceRequest::Configuration => {
                            data.config().map_err(|error| state_failure(&error))
                        }
                        ServiceRequest::StateRead { key } => data
                            .read(&key)
                            .map(Option::unwrap_or_default)
                            .map_err(|error| state_failure(&error)),
                        ServiceRequest::StateWrite { key, value } => data
                            .write(&key, &value)
                            .map(|()| Value::Null)
                            .map_err(|error| state_failure(&error)),
                        ServiceRequest::Tool { .. } => Err(Failure::new("permission_required")),
                        _ => Err(Failure::new("undeclared_service")),
                    },
                )
                .map_err(|error| state_failure(&error))?
        })
    }
}

fn state_failure(error: &ene_store::StoreError) -> Failure {
    match error {
        ene_store::StoreError::Permission(_) => Failure::new("provider_revoked"),
        _ => Failure::new("service_failed"),
    }
}

/// Resolved per-connection invocation context shared across LLM, embedding,
/// and speech provider plugins.
#[derive(Clone)]
pub(crate) struct ProviderInvocationContext {
    provider_id: Arc<str>,
    pub(crate) endpoint: String,
    pub(crate) options: Value,
    broker: Arc<HttpBroker>,
}

impl ProviderInvocationContext {
    /// Builds an invocation context for one provider plugin and connection,
    /// constructing its HTTP broker from `Grant`.
    pub(crate) fn from_connection(
        running: &super::Running,
        connection: &crate::host::settings::Connection,
        secret: Option<String>,
        providers: &super::Providers,
        deadline: Option<std::time::Duration>,
    ) -> crate::host::error::Result<Self> {
        let role = running.provider().ok_or_else(|| {
            crate::host::error::HostError::Unavailable(
                ("the extension serves no provider to call").into(),
            )
        })?;
        let grant = super::Grant::new(&role.configuration, secret);
        let broker =
            broker(Arc::new(grant), deadline, running.admission_lifetime()).map_err(|error| {
                crate::host::error::HostError::Unavailable(format!(
                    "plugin HTTP transport: {error}"
                ))
            })?;
        Ok(Self::with_broker(role, connection, providers, broker))
    }

    /// Builds an invocation context using a caller-provided broker.
    ///
    /// It takes the provider role rather than `Running`, so the harness case
    /// cannot reach it.
    pub(crate) fn with_broker(
        role: &super::ProviderRole,
        connection: &crate::host::settings::Connection,
        providers: &super::Providers,
        broker: Arc<HttpBroker>,
    ) -> Self {
        let endpoint = crate::host::settings::endpoint_of(connection, providers)
            .unwrap_or_else(|| role.configuration.endpoint.clone());
        let options = merge_options(&role.configuration.options, &connection.endpoint().options);
        Self {
            provider_id: role.id.to_string().into(),
            endpoint,
            options,
            broker,
        }
    }

    /// Creates a fresh reply namespace for one guest invocation.
    pub(crate) fn services(&self) -> Arc<BrokerServices> {
        Arc::new(BrokerServices {
            broker: self.broker.for_invocation(),
            provider: Arc::clone(&self.provider_id),
        })
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
            merged.extend(override_map);
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
        Box::pin(async move {
            Ok(match request {
                ServiceRequest::HttpOpen { request } => {
                    serde_json::to_value(self.broker.open(&self.provider, request).await?)
                        .map_err(|_| Failure::new("serialize"))?
                }
                ServiceRequest::HttpRead { handle } => {
                    serde_json::to_value(self.broker.read(&handle).await?)
                        .map_err(|_| Failure::new("serialize"))?
                }
                ServiceRequest::HttpClose { handle } => {
                    self.broker.close(&handle)?;
                    Value::Null
                }
                // Audio is read by the host from its own synthesis answer, not
                // by the guest, so a plugin never asks for it.
                _ => return Err(Failure::new("undeclared_service")),
            })
        })
    }
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { self.broker.check() })
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
