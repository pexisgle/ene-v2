//! The owner credential is the only host authentication boundary. Every
//! authenticated client has the same host rights; device operations name their
//! device in the request instead of carrying a second credential.
use super::{error::ApiError, state::App};
use axum::{
    extract::State,
    http::{HeaderMap, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn owner(app: &App, headers: &HeaderMap) -> bool {
    bearer(headers).is_some_and(|v| bool::from(v.as_bytes().ct_eq(app.token.as_bytes())))
}

pub(crate) async fn authenticate(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    if !owner(&app, headers) {
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
    }
    next.run(request).await
}
