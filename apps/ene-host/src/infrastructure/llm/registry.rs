//! Builtin and reviewed Wasm providers share the host authorization port.
use super::openai;
use crate::{
    domain::{Connection, Error, ErrorKind},
    llm::*,
    ports::Result,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;
pub struct Registry {
    plugins: Option<Arc<crate::infrastructure::plugins::Manager>>,
}
impl Registry {
    #[cfg(test)]
    pub fn discover() -> Self {
        Self::with_plugins(None)
    }
    pub fn with_plugins(plugins: Option<Arc<crate::infrastructure::plugins::Manager>>) -> Self {
        Self { plugins }
    }
    fn plugins(&self) -> Result<&crate::infrastructure::plugins::Manager> {
        self.plugins
            .as_deref()
            .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))
    }
}
impl ProviderRegistry for Registry {
    fn descriptors(&self) -> Vec<Descriptor> {
        let mut result = vec![openai::descriptor()];
        if let Some(plugins) = &self.plugins {
            result.extend(plugins.descriptors());
        }
        result
    }
    fn policy(&self, connection: &Connection) -> Result<Policy> {
        if connection.adapter == openai::ID {
            return openai::policy(connection).map(|p| p.bind_connection(connection));
        }
        self.plugins()?
            .provider_policy(connection)
            .map(|p| p.bind_connection(connection))
    }
    fn scope_fingerprint(&self, connection: &Connection) -> Result<serde_json::Value> {
        if connection.adapter == openai::ID {
            return self.policy(connection).map(|p| p.fingerprint);
        }
        self.plugins()?
            .provider_identity(connection)
            .map(|p| p.bind_connection(connection).fingerprint)
    }
    fn resolve(&self, connection: &Connection) -> Result<ProviderBinding> {
        if connection.adapter == openai::ID {
            return ProviderBinding::resolve(Arc::new(openai::Provider), connection);
        }
        ProviderBinding::resolve(
            self.plugins()?
                .provider(&connection.adapter)
                .ok_or_else(Error::unavailable)?,
            connection,
        )
    }
    fn reload(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            if let Some(plugins) = &self.plugins {
                use crate::ports::plugin::Plugins;
                plugins.reload().await?;
            }
            Ok(())
        })
    }
    fn enable(&self, id: String, enabled: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if id == openai::ID {
                return if enabled {
                    Ok(())
                } else {
                    Err(Error::new(ErrorKind::UnsupportedCapability))
                };
            }
            self.plugins()?.enable_provider(&id, enabled).await
        })
    }
}
