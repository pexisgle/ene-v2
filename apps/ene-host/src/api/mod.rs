//! Compose versioned APIs, then apply authentication to routes and fallbacks.
mod auth;
pub(crate) mod error;
mod extractors;
pub(crate) mod state;
#[cfg(test)]
mod tests;
mod v1;

use crate::{api::error::ApiError, api::state::App};
use axum::{Router, http::StatusCode, middleware};

pub(crate) fn router(app: App) -> Router {
    Router::new()
        .nest("/v1", v1::router())
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "未対応のAPIです")
        })
        // `layer` also protects unknown paths; `route_layer` would bypass them.
        .layer(middleware::from_fn_with_state(
            app.clone(),
            auth::authenticate,
        ))
        .with_state(app)
}
