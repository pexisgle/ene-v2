//! Host RPC orchestration and epoch-scoped update dispatch for the desktop client.

// Captures the request while keeping connection acquisition in the dispatcher.
macro_rules! request {
    ($service:ident . $method:ident($request:expr)) => {
        |host| async move { host.$service.$method($request).await }
    };
}

// Empty unary reads differ only by method, label, and update mapping.
macro_rules! load {
    ($($name:ident: $service:ident.$method:ident($context:literal) => $done:expr;)+) => {
        $(pub(crate) fn $name(rpc: &$crate::rpc::RpcDispatcher, epoch: u64) {
            rpc.call_owned(epoch, $context, request!($service.$method(::ene_rpc::Empty::default())), $done);
        })+
    };
}

pub mod chat;
pub mod companions;
pub mod connections;
pub mod policy;

use crate::{host::Host, session::Connection, update::Update};
use buffa::view::{MessageView, OwnedView};
use connectrpc::{ConnectError, client::UnaryResponse};
use ene_client_core::{
    domain::HostNotification,
    state::{Notice, NoticeKind},
};
use ene_rpc as w;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// How long one RPC may take before the UI gives up on it.
///
/// Generous next to a loopback round trip: this exists to stop a wedged host
/// from hanging the window, not to police normal latency.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct CallContext {
    label: &'static str,
    timeout: Duration,
}

impl From<&'static str> for CallContext {
    fn from(label: &'static str) -> Self {
        Self {
            label,
            timeout: RPC_TIMEOUT,
        }
    }
}

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

    /// Runs a unary request with the default failure update.
    pub(crate) fn call<T, F>(
        &self,
        epoch: u64,
        context: &'static str,
        run: impl FnOnce(Host) -> F,
        done: impl FnOnce(T) -> Update + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, ConnectError>> + Send + 'static,
    {
        self.call_with(epoch, context, run, done, failure);
    }

    /// Opens a unary response's owned message only if the update uses it.
    pub(crate) fn call_owned<V, F>(
        &self,
        epoch: u64,
        context: &'static str,
        run: impl FnOnce(Host) -> F,
        done: impl FnOnce(V::Owned) -> Update + Send + 'static,
    ) where
        V: MessageView<'static> + Send + 'static,
        F: Future<Output = Result<UnaryResponse<OwnedView<V>>, ConnectError>> + Send + 'static,
    {
        self.call(epoch, context, run, move |response| {
            done(response.into_owned())
        });
    }

    fn call_with<T, F>(
        &self,
        epoch: u64,
        context: impl Into<CallContext>,
        run: impl FnOnce(Host) -> F,
        done: impl FnOnce(T) -> Update + Send + 'static,
        error: impl FnOnce(&'static str, &ConnectError) -> Update + Send + 'static,
    ) -> bool
    where
        T: Send + 'static,
        F: Future<Output = Result<T, ConnectError>> + Send + 'static,
    {
        let Some(host) = self.host() else {
            return false;
        };
        self.dispatch(epoch, context.into(), run(host), done, error);
        true
    }

    fn dispatch<T>(
        &self,
        epoch: u64,
        context: CallContext,
        future: impl Future<Output = Result<T, ConnectError>> + Send + 'static,
        done: impl FnOnce(T) -> Update + Send + 'static,
        error: impl FnOnce(&'static str, &ConnectError) -> Update + Send + 'static,
    ) where
        T: Send + 'static,
    {
        let updates = self.updates.clone();
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        self.runtime.spawn(async move {
            let inner = match tokio::time::timeout(context.timeout, future).await {
                Ok(Ok(value)) => done(value),
                Ok(Err(failed)) => error(context.label, &failed),
                Err(_) => error(
                    context.label,
                    &ConnectError::deadline_exceeded("the host did not answer in time"),
                ),
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

    pub(crate) fn call_update<F>(&self, epoch: u64, run: impl FnOnce(Host) -> F) -> bool
    where
        F: Future<Output = Update> + Send + 'static,
    {
        self.call_with(
            epoch,
            "host request",
            |host| {
                let future = run(host);
                async move { Ok(future.await) }
            },
            std::convert::identity,
            |context, _| Update::Failed {
                context,
                message: "ホストが時間内に応答しませんでした。接続を確認してください。".into(),
            },
        )
    }
}

pub(crate) fn failure(context: &'static str, error: &ConnectError) -> Update {
    let message = error.to_string();
    match failure_kind(error) {
        NoticeKind::Conflict => Update::Conflict { context, message },
        NoticeKind::Denied => Update::Denied { context, message },
        NoticeKind::Failed => Update::Failed { context, message },
    }
}

fn failure_notice(context: &'static str, error: &ConnectError) -> Notice {
    Notice {
        context: context.into(),
        message: error.to_string(),
        kind: failure_kind(error),
    }
}

fn failure_kind(error: &ConnectError) -> NoticeKind {
    use connectrpc::ErrorCode;
    match error.code {
        ErrorCode::Aborted => NoticeKind::Conflict,
        ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => NoticeKind::Denied,
        _ => NoticeKind::Failed,
    }
}

pub(crate) fn companion_update(companion: w::Companion) -> Update {
    Update::Notification(Box::new(HostNotification::companion(companion.into())))
}

fn id_request(id: String) -> w::Id {
    w::Id {
        id,
        ..Default::default()
    }
}
