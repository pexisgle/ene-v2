//! Host RPC orchestration and epoch-scoped update dispatch for the desktop client.

pub mod chat;
pub mod companions;
pub mod connections;
pub mod policy;

use crate::{host::Host, session::Connection, update::Update};
use ene_client_core::domain::HostNotification;
use ene_rpc as w;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedSender;

/// Dispatches asynchronous Connect RPC calls on the session Tokio runtime and routes scoped updates back to the UI.
#[derive(Debug)]
pub(crate) struct RpcDispatcher {
    connection: Connection,
    runtime: tokio::runtime::Handle,
    updates: UnboundedSender<Update>,
    request_seq: AtomicU64,
}

impl RpcDispatcher {
    pub(crate) fn new(
        connection: Connection,
        runtime: tokio::runtime::Handle,
        updates: UnboundedSender<Update>,
    ) -> Self {
        Self {
            connection,
            runtime,
            updates,
            request_seq: AtomicU64::new(1),
        }
    }

    pub(crate) fn host(&self) -> Option<Host> {
        self.connection.read().ok()?.clone()
    }

    pub(crate) fn updates(&self) -> UnboundedSender<Update> {
        self.updates.clone()
    }

    pub(crate) fn spawn<T, D>(
        &self,
        epoch: u64,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
    {
        self.spawn_with(epoch, context, future, done, failure);
    }

    pub(crate) fn spawn_with<T, D, E>(
        &self,
        epoch: u64,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
        error: E,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
        E: FnOnce(&'static str, connectrpc::ConnectError) -> Update + Send + 'static,
    {
        let updates = self.updates.clone();
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        self.runtime.spawn(async move {
            let inner = match future.await {
                Ok(value) => done(value),
                Err(failed) => error(context, failed),
            };
            crate::session::notify_ui(
                &updates,
                Update::Scoped {
                    epoch,
                    seq,
                    inner: Box::new(inner),
                },
            );
        });
    }

    pub(crate) fn spawn_update(
        &self,
        epoch: u64,
        future: impl Future<Output = Update> + Send + 'static,
    ) {
        let updates = self.updates.clone();
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        self.runtime.spawn(async move {
            let inner = future.await;
            crate::session::notify_ui(
                &updates,
                Update::Scoped {
                    epoch,
                    seq,
                    inner: Box::new(inner),
                },
            );
        });
    }
}

pub(crate) fn decode(error: buffa::DecodeError) -> connectrpc::ConnectError {
    connectrpc::ConnectError::internal(error.to_string())
}

pub(crate) fn failure(context: &'static str, error: connectrpc::ConnectError) -> Update {
    match error.code {
        connectrpc::ErrorCode::Aborted => Update::Conflict {
            context,
            message: error.to_string(),
        },
        connectrpc::ErrorCode::Unauthenticated | connectrpc::ErrorCode::PermissionDenied => {
            Update::Denied {
                context,
                message: error.to_string(),
            }
        }
        _ => Update::Failed {
            context,
            message: error.to_string(),
        },
    }
}

pub(crate) fn settings_update(settings: w::Settings) -> Update {
    Update::Notification(Box::new(HostNotification::settings_updated(
        settings.into(),
    )))
}

pub(crate) fn companion_update(companion: w::Companion) -> Update {
    Update::Notification(Box::new(HostNotification::companion(companion.into())))
}
