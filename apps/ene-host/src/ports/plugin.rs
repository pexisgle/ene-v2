//! Plugin management stays independent of guest runtimes and platform services.
use super::Result;
use ene_rpc::{EnablePluginRequest, Plugin, PluginBinding, PluginInvocation, PluginResult};
use futures_util::future::BoxFuture;
use std::sync::Arc;

/// Accepted guest mutations outlive the invocation waiting for them.
pub trait Mutations: Send + Sync {
    fn submit(&self, operation: BoxFuture<'static, Result<()>>) -> BoxFuture<'_, Result<()>>;
}

pub trait Plugins: Send + Sync {
    fn list(&self) -> Vec<Plugin>;
    fn reload(&self) -> BoxFuture<'_, Result<()>>;
    fn enable(&self, id: String, input: EnablePluginRequest) -> BoxFuture<'_, Result<()>>;
    fn bindings(&self) -> BoxFuture<'_, Result<Vec<PluginBinding>>>;
    fn bind(&self, input: PluginBinding) -> BoxFuture<'_, Result<()>>;
    fn invoke(
        &self,
        input: PluginInvocation,
        mutations: Arc<dyn Mutations>,
    ) -> BoxFuture<'_, Result<PluginResult>>;
    fn restore(&self) -> BoxFuture<'_, Result<()>>;
    fn cancel(&self);
    fn shutdown(&self) -> BoxFuture<'_, Result<()>>;
}
