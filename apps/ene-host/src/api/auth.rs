//! One authorization boundary for native credentials and approved devices.
use super::{error::ApiError, state::App};
use axum::{
    extract::State,
    http::{HeaderMap, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) enum Principal {
    Owner,
    Device(String, CancellationToken),
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

pub(crate) fn owner(app: &App, headers: &HeaderMap) -> bool {
    bearer(headers).is_some_and(|v| bool::from(v.as_bytes().ct_eq(app.token.as_bytes())))
}

pub(crate) async fn authenticate(
    State(app): State<App>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let principal = if owner(&app, headers) {
        Some(Principal::Owner)
    } else if let Some(token) = bearer(headers) {
        app.devices
            .authenticate(token)
            .await
            .map(|(id, changed)| Principal::Device(id, changed))
    } else {
        None
    };
    let Some(principal) = principal else {
        if request.uri().path().starts_with("/rpc/") {
            return connectrpc::ConnectError::unauthenticated("認証が必要です")
                .into_http_response(headers)
                .map(axum::body::Body::new);
        }
        return ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "unauthorized",
            "認証が必要です",
        )
        .into_response();
    };
    if matches!(principal, Principal::Device(_, _))
        && !matches!(
            request.uri().path(),
            "/rpc/ene.host.v1.HostService/GetSnapshot"
                | "/rpc/ene.host.v1.HostService/Subscribe"
                | "/rpc/ene.host.v1.HostService/ListCompanions"
                | "/rpc/ene.host.v1.HostService/GetCompanion"
                | "/rpc/ene.host.v1.HostService/GetHistory"
                | "/rpc/ene.host.v1.HostService/GetMessage"
                | "/rpc/ene.host.v1.HostService/SendMessage"
                | "/rpc/ene.host.v1.HostService/Stop"
                | "/rpc/ene.host.v1.HostService/GetAvailability"
                | "/rpc/ene.host.v1.DeviceService/Subscribe"
                | "/rpc/ene.host.v1.DeviceService/CompleteOperation"
                | "/rpc/ene.host.v1.DeviceService/GetOperation"
                | "/rpc/ene.host.v1.DeviceService/Activate"
        )
    {
        return connectrpc::ConnectError::permission_denied("管理権限が必要です")
            .into_http_response(headers)
            .map(axum::body::Body::new);
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}

pub(crate) fn own_device(
    ctx: &connectrpc::RequestContext,
    id: &str,
) -> Result<(), connectrpc::ConnectError> {
    match ctx.extensions().get::<Principal>() {
        Some(Principal::Device(own, _)) if own != id => Err(
            connectrpc::ConnectError::permission_denied("別の端末は操作できません"),
        ),
        Some(_) => Ok(()),
        None => Err(connectrpc::ConnectError::unauthenticated("認証が必要です")),
    }
}

pub(crate) async fn cancelled(principal: Principal) {
    match principal {
        Principal::Device(_, changed) => changed.cancelled().await,
        Principal::Owner => std::future::pending::<()>().await,
    }
}
