//! Connect service implementations and the owner-authentication boundary.

/// Declares a service method that hands its owned request to one host
/// operation. The first form selects the read-only lane, `app` passes the
/// `App` itself instead of its world, and the bare form is a write on the
/// world; the expansion keeps the connectrpc call/read shape.
macro_rules! request {
    (read $name:ident, $request:ty, $response:ty, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_message();
            self.read(move |app| $op(app.world_mut(), request)).await
        }
    };
    (app $name:ident, $request:ty, $response:ty, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_message();
            self.call(move |app| $op(app, request)).await
        }
    };
    ($name:ident, $request:ty, $response:ty, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_message();
            self.call(move |app| $op(app.world_mut(), request)).await
        }
    };
}

/// Declares a write method that answers `Empty` once the operation succeeded.
macro_rules! discard {
    ($name:ident, $request:ty, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
            let request = request.to_owned_message();
            self.call(move |app| {
                $op(app.world_mut(), request)?;
                Ok(ene_rpc::Empty::default())
            })
            .await
        }
    };
}

/// Declares a read method addressed by `Id` that hands the id to one store
/// operation.
macro_rules! read_id {
    ($name:ident, $response:ty, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, ene_rpc::Id>,
        ) -> connectrpc::ServiceResult<$response> {
            let id = request.id.to_owned();
            self.read(move |app| $op(app.world().resource::<crate::host::db::Store>(), &id))
                .await
        }
    };
}

/// Declares a write method addressed by one id field that answers `Empty`.
macro_rules! discard_id {
    ($name:ident, $request:ty, $field:ident, $op:path) => {
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
            let id = request.$field.to_owned();
            self.call(move |app| {
                $op(app.world_mut(), &id)?;
                Ok(ene_rpc::Empty::default())
            })
            .await
        }
    };
}

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

pub(super) fn router(
    runtime: Runtime,
    guard: auth::Guard,
    shutdown: CancellationToken,
) -> axum::Router {
    let services = Arc::new(Services { runtime, shutdown });
    let connect = w::HostServiceExt::register(services.clone(), Router::new());
    let connect = w::ChatServiceExt::register(services.clone(), connect);
    let connect = w::SettingsServiceExt::register(services.clone(), connect);
    let connect = w::WorkServiceExt::register(services.clone(), connect);
    let connect = w::ClientServiceExt::register(services.clone(), connect);
    let connect = w::VoiceServiceExt::register(services, connect);
    axum::Router::new()
        .nest_service("/rpc", connect.into_axum_router())
        .layer(middleware::from_fn_with_state(guard, auth::authenticate))
}
