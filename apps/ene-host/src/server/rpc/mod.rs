//! Connect service implementations and the owner-authentication boundary.

/// Declares a host operation. `read_view` retains the byte owner until the
/// queued read borrows it. `read` materializes an owned message, `app` passes
/// the `App`, and the bare form writes to the world.
macro_rules! request {
    (read_view $name:ident, $request:ty, $response:ty, $op:path) => {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_view();
            self.read(move |app| $op(app.world_mut(), request.reborrow()))
                .await
        }
    };
    (read $name:ident, $request:ty, $response:ty, $op:path) => {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_message();
            self.read(move |app| $op(app.world_mut(), &request)).await
        }
    };
    (store $name:ident, $request:ty, $response:ty, $op:path) => {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, $request>,
        ) -> connectrpc::ServiceResult<$response> {
            let request = request.to_owned_message();
            // `$op` takes only the store, so this never needs the actor thread.
            self.with_store(move |store| $op(store, request)).await
        }
    };
    (app $name:ident, $request:ty, $response:ty, $op:path) => {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
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
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
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
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
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
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
        async fn $name(
            &self,
            _: connectrpc::RequestContext,
            request: connectrpc::ServiceRequest<'_, ene_rpc::Id>,
        ) -> connectrpc::ServiceResult<$response> {
            let id = request.id.to_owned();
            // `$op` takes only the store, so this never needs the actor thread.
            self.with_store(move |store| $op(&store, &id)).await
        }
    };
}

/// Declares a write method addressed by one id field that answers `Empty`.
macro_rules! discard_id {
    ($name:ident, $request:ty, $field:ident, $op:path) => {
        #[tracing::instrument(name = "rpc", skip_all, fields(method = stringify!($name), companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
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
mod voice_convert;
mod work;

use super::auth;
use crate::host::{
    Runtime,
    error::{ErrorCode, HostError},
};
use axum::middleware;
use connectrpc::{ConnectError, Response, Router, ServiceResult};
use ene_rpc as w;
use rig_ecs::prelude::App;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower_http::{
    catch_panic::CatchPanicLayer, limit::RequestBodyLimitLayer, timeout::TimeoutLayer,
    trace::TraceLayer,
};

/// Largest request body the Connect surface accepts.
///
/// The biggest legitimate body is a perception frame: a JPEG capped at 512 KiB
/// by `docs/api.md` §知覚. 8 MiB leaves room for a batch of frames plus
/// protobuf overhead while still refusing a malformed or hostile body before it
/// is buffered in memory.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How long one RPC may run before the layer answers 408.
///
/// Generous on purpose: `SettingsService` probes a provider connection and
/// `WorkService` starts a session, and both legitimately wait on a remote model.
/// The per-call deadlines that matter are set inside those handlers; this is
/// only a backstop for a handler that never returns at all.
const RPC_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(super) struct Services {
    pub(super) runtime: Runtime,
    pub(super) shutdown: CancellationToken,
}

impl Services {
    /// The store, held by the runtime, for operations that never need the world.
    pub(super) fn store(&self) -> crate::host::db::Store {
        self.runtime.store()
    }

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

    /// Runs an operation that touches only the store, off the ECS thread.
    ///
    /// The actor thread also drives every queued command and the ready check,
    /// so a handler that only reads SQLite has no reason to queue behind it.
    /// Readiness is still checked first, because a host that has failed must
    /// not answer reads either.
    pub(super) async fn with_store<T: Send + 'static>(
        &self,
        operation: impl FnOnce(crate::host::db::Store) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        self.runtime.ready().await.map_err(ConnectError::from)?;
        let store = self.store();
        Response::ok(operation(store).map_err(ConnectError::from)?)
    }
}

impl From<HostError> for ConnectError {
    fn from(error: HostError) -> Self {
        // One lookup decides the status, so a new `ErrorCode` variant cannot be
        // mapped inconsistently here and in `HostError::code`.
        let message = error.message().to_owned();
        match error.code() {
            ErrorCode::InvalidArgument => ConnectError::invalid_argument(message),
            ErrorCode::NotFound => ConnectError::not_found(message),
            ErrorCode::Aborted => ConnectError::aborted(message),
            ErrorCode::AlreadyExists => ConnectError::already_exists(message),
            ErrorCode::PermissionDenied => ConnectError::permission_denied(message),
            ErrorCode::ResourceExhausted => ConnectError::resource_exhausted(message),
            ErrorCode::Unavailable => ConnectError::unavailable(message),
            ErrorCode::FailedPrecondition => ConnectError::failed_precondition(message),
            ErrorCode::Storage => {
                ConnectError::unavailable("state storage failed; restart after repairing storage")
            }
            ErrorCode::Internal => ConnectError::internal(message),
        }
    }
}

/// Liveness: the process is running and serving HTTP at all.
async fn healthz() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}

/// Readiness: the host kernel can accept RPC calls.
///
/// It asks the same `HostState::ready` the RPC layer asks, so a 200 here means
/// the host would not refuse a call for being failed or closing.
async fn readyz(
    axum::extract::State(services): axum::extract::State<Arc<Services>>,
) -> axum::http::StatusCode {
    match services.runtime.ready().await {
        Ok(()) => axum::http::StatusCode::OK,
        Err(error) => {
            tracing::debug!(%error, "readiness probe failed");
            axum::http::StatusCode::SERVICE_UNAVAILABLE
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
    let connect = w::VoiceServiceExt::register(services.clone(), connect);
    // The RPC surface carries the auth layer; the probes do not. A load
    // balancer or supervisor has no Bearer token, and a health check that
    // needs one cannot report that the host is down.
    let authenticated = axum::Router::new()
        .nest_service("/rpc", connect.into_axum_router())
        .layer(middleware::from_fn_with_state(guard, auth::authenticate));
    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/readyz", axum::routing::get(readyz))
        .with_state(services)
        .merge(authenticated)
        // A panic in one handler must not take the process down; the layer
        // turns it into a 500 and leaves the actor thread intact.
        .layer(CatchPanicLayer::new())
        // A Connect request larger than this is refused before it is buffered.
        // Perception frames are capped well below it by the client's own JPEG
        // limit, so this only stops a malformed or hostile body.
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // A handler that never returns must not pin a connection forever.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            RPC_TIMEOUT,
        ))
        // Emits one span per request, which the per-method `#[instrument]`
        // spans then nest inside.
        .layer(TraceLayer::new_for_http())
}
