//! Host RPC orchestration and epoch-scoped update dispatch for the desktop client.

// Captures the request while keeping connection acquisition in the dispatcher.
macro_rules! request {
    ($service:ident . $method:ident($request:expr)) => {
        |host| async move { host.$service.$method($request).await }
    };
}

pub mod chat;
pub mod companions;
pub mod connections;
pub mod policy;

use crate::{
    host::{Host, RPC_TIMEOUT},
    session::Connection,
    update::Update,
};
use buffa::view::{MessageView, OwnedView};
use connectrpc::{ConnectError, client::UnaryResponse};
use ene_client_core::{
    ReadTarget,
    domain::HostNotification,
    state::{Notice, NoticeKind},
};
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

    pub(crate) fn has_host(&self) -> bool {
        self.connection.read().is_some()
    }

    fn unary_host(&self) -> Option<Host> {
        let mut host = self.connection.read().clone()?;
        // The session keeps its original clients for long-lived streams.
        for config in [
            host.host.config_mut(),
            host.chat.config_mut(),
            host.settings.config_mut(),
        ] {
            *config = config.clone().with_default_timeout(RPC_TIMEOUT);
        }
        Some(host)
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
        context: &'static str,
        run: impl FnOnce(Host) -> F,
        done: impl FnOnce(T) -> Update + Send + 'static,
        error: impl FnOnce(&'static str, &ConnectError) -> Update + Send + 'static,
    ) -> bool
    where
        T: Send + 'static,
        F: Future<Output = Result<T, ConnectError>> + Send + 'static,
    {
        let Some(host) = self.unary_host() else {
            return false;
        };
        self.dispatch(epoch, context, run(host), done, error);
        true
    }

    fn dispatch<T>(
        &self,
        epoch: u64,
        context: &'static str,
        future: impl Future<Output = Result<T, ConnectError>> + Send + 'static,
        done: impl FnOnce(T) -> Update + Send + 'static,
        error: impl FnOnce(&'static str, &ConnectError) -> Update + Send + 'static,
    ) where
        T: Send + 'static,
    {
        let updates = self.updates.clone();
        let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
        self.runtime.spawn(async move {
            let inner = match future.await {
                Ok(value) => done(value),
                Err(failed) => error(context, &failed),
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

pub(crate) fn failure(context: &'static str, error: &ConnectError) -> Update {
    Update::Notice {
        notice: failure_notice(context, error),
        read: None,
    }
}

fn read_failure(
    target: ReadTarget,
) -> impl FnOnce(&'static str, &ConnectError) -> Update + Send + 'static {
    move |context, error| Update::Notice {
        read: Some(target),
        notice: failure_notice(context, error),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ene_client_core::{Model, domain::ModelInfo};
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn late_model_list_failure_does_not_show_after_newer_success() {
        let (updates, mut received) = mpsc::unbounded_channel();
        let dispatcher = RpcDispatcher::new(
            Connection::default(),
            tokio::runtime::Handle::current(),
            updates,
        );
        let mut model = Model::new("fixture".into());
        let epoch = model.connections.link_epoch;
        let (release, blocked) = oneshot::channel();
        let target = ReadTarget::Models("llm".into());

        dispatcher.dispatch(
            epoch,
            "モデル一覧の取得",
            async move {
                blocked.await.unwrap();
                Err::<(), _>(ConnectError::internal("earlier request failed"))
            },
            |()| unreachable!(),
            read_failure(target.clone()),
        );
        dispatcher.dispatch(
            epoch,
            "モデル一覧の取得",
            async { Ok(()) },
            |()| Update::Models {
                connection_id: "llm".into(),
                models: vec![ModelInfo {
                    id: "current".into(),
                    name: "Current".into(),
                }],
            },
            read_failure(target),
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            model.apply(received.recv().await.unwrap());
            assert_eq!(model.connections.models["llm"][0].id, "current");
            release.send(()).unwrap();
            model.apply(received.recv().await.unwrap());
            assert_eq!(model.notices.len(), 0);
        })
        .await
        .unwrap();
    }
}
