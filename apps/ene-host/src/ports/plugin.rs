//! Plugin management stays independent of guest runtimes and platform services.
use super::Result;
use futures_util::future::BoxFuture;

/// Accepted guest mutations outlive the invocation waiting for them.
pub trait Mutations: Send + Sync {
    fn submit(&self, operation: BoxFuture<'static, Result<()>>) -> BoxFuture<'_, Result<()>>;
}
