//! Connect service implementations and the owner-authentication boundary.
mod chat;
mod client;
mod host;
mod settings;
mod voice;
mod work;

use super::auth;
use crate::host::{Runtime, error::HostError};
use axum::middleware;
use connectrpc::{ConnectError, Response, Router, ServiceResult};
use ene_rpc as w;
use rig_ecs::prelude::App;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Services {
    pub(super) runtime: Runtime,
    pub(super) shutdown: CancellationToken,
}

impl Services {
    pub(super) async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .call(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }

    pub(super) async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut App) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .read(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }
}

impl From<HostError> for ConnectError {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Invalid(message) => ConnectError::invalid_argument(message),
            HostError::NotFound(message) => ConnectError::not_found(message),
            HostError::Conflict(message) => ConnectError::aborted(message),
            HostError::AlreadyExists(message) => ConnectError::already_exists(message),
            HostError::Permission(message) => ConnectError::permission_denied(message),
            HostError::Exhausted(message) => ConnectError::resource_exhausted(message),
            HostError::Unavailable(message) => ConnectError::unavailable(message),
            HostError::Storage(_) => {
                ConnectError::unavailable("state storage failed; restart after repairing storage")
            }
            HostError::Internal(message) => ConnectError::internal(message),
        }
    }
}

pub(super) fn router(runtime: Runtime, token: String, shutdown: CancellationToken) -> axum::Router {
    let services = Arc::new(Services { runtime, shutdown });
    let connect = w::HostServiceExt::register(services.clone(), Router::new());
    let connect = w::ChatServiceExt::register(services.clone(), connect);
    let connect = w::SettingsServiceExt::register(services.clone(), connect);
    let connect = w::WorkServiceExt::register(services.clone(), connect);
    let connect = w::ClientServiceExt::register(services.clone(), connect);
    let connect = w::VoiceServiceExt::register(services, connect);
    axum::Router::new()
        .nest_service("/rpc", connect.into_axum_router())
        .layer(middleware::from_fn_with_state(token, auth::authenticate))
}
