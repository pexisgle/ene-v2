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

/// How long one RPC may take before the UI gives up on it.
///
/// Generous next to a loopback round trip: this exists to stop a wedged host
/// from hanging the window, not to police normal latency.
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
        self.connection.read().clone()
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
            // Without a deadline a request the host accepted but never answers
            // leaves the UI waiting for the life of the process; the
            // transport's timeout covers connection setup, not this.
            let outcome = match tokio::time::timeout(RPC_TIMEOUT, future).await {
                Ok(result) => result,
                Err(_) => Err(connectrpc::ConnectError::deadline_exceeded(
                    "the host did not answer in time",
                )),
            };
            let inner = match outcome {
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
            let inner = match tokio::time::timeout(RPC_TIMEOUT, future).await {
                Ok(inner) => inner,
                Err(_) => Update::Failed {
                    context: "host request",
                    message: "ホストが時間内に応答しませんでした。接続を確認してください。".into(),
                },
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
