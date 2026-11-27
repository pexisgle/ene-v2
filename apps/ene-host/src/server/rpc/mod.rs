//! Connect service implementations and the owner-authentication boundary.

mod chat;
mod client;
mod host;
mod settings;
mod voice;
mod voice_convert;
mod work;

use super::auth;
use crate::host::{HostServices, Runtime, agent::AgentKernel, error::HostError};
use axum::middleware;
use connectrpc::{ConnectError, Response, Router, ServiceResult};
use ene_domain::ClientId;
use ene_rpc as w;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower_http::{
    catch_panic::CatchPanicLayer,
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, MakeSpan as _, TraceLayer},
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
        operation: impl FnOnce(&mut AgentKernel<'_>) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .call_kernel(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }

    pub(super) async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut AgentKernel<'_>) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .read_kernel(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }

    pub(super) async fn change<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&HostServices) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .change(operation)
                .await
                .map_err(ConnectError::from)?,
        )
    }

    pub(super) async fn inspect<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&HostServices) -> crate::host::error::Result<T> + Send + 'static,
    ) -> ServiceResult<T> {
        Response::ok(
            self.runtime
                .inspect(operation)
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
        self.runtime.ready().map_err(ConnectError::from)?;
        let store = self.store();
        Response::ok(operation(store).map_err(ConnectError::from)?)
    }
}

fn parse_client_id(id: &str) -> Result<ClientId, ConnectError> {
    ClientId::parse(id).map_err(|error| ConnectError::invalid_argument(error.to_string()))
}

impl From<HostError> for ConnectError {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Invalid(message) => Self::invalid_argument(message),
            HostError::Domain(error) => Self::invalid_argument(error.to_string()),
            HostError::NotFound(message) => Self::not_found(message),
            HostError::Conflict(message) => Self::aborted(message),
            HostError::AlreadyExists(message) => Self::already_exists(message),
            HostError::Permission(message) => Self::permission_denied(message),
            HostError::Exhausted(message) => Self::resource_exhausted(message),
            HostError::Unavailable(message) => Self::unavailable(message),
            HostError::FailedPrecondition(message) => Self::failed_precondition(message),
            HostError::Storage(_) => {
                Self::unavailable("state storage failed; restart after repairing storage")
            }
            HostError::Internal(message) => Self::internal(message),
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
    match services.runtime.ready() {
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
        .layer(TraceLayer::new_for_http().make_span_with(request_span))
}

fn request_span(request: &http::Request<axum::body::Body>) -> tracing::Span {
    if let Some(method) = request
        .uri()
        .path()
        .strip_prefix("/rpc/ene.host.v1.SettingsService/")
        && !method.is_empty()
        && method.len() <= 64
        && method.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return settings::procedure_span(method);
    }
    DefaultMakeSpan::new().make_span(request)
}
