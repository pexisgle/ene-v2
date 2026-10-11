//! Plugin management stays independent of guest runtimes and platform services.
use super::Result;
use ene_rpc::{EnablePluginRequest, Plugin, PluginBinding, PluginInvocation, PluginResult};
use futures_util::future::BoxFuture;
pub trait Plugins: Send + Sync {
    fn list(&self) -> Vec<Plugin>;
    fn reload(&self) -> BoxFuture<'_, Result<()>>;
    fn enable(&self, id: String, input: EnablePluginRequest) -> BoxFuture<'_, Result<()>>;
    fn bindings(&self) -> BoxFuture<'_, Result<Vec<PluginBinding>>>;
    fn bind(&self, input: PluginBinding) -> BoxFuture<'_, Result<()>>;
    fn invoke(&self, input: PluginInvocation) -> BoxFuture<'_, Result<PluginResult>>;
    fn restore(&self) -> BoxFuture<'_, ()>;
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}
