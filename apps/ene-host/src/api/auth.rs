use crate::{api::error::ApiError, api::state::App};
use axum::{
    extract::State,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

pub(crate) async fn authenticate(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let allowed =
        provided.is_some_and(|value| bool::from(value.as_bytes().ct_eq(app.token.as_bytes())));
    if !allowed {
        return ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "認証が必要です")
            .into_response();
    }
    next.run(request).await
}
