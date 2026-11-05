//! Compose versioned APIs, then apply authentication to routes and fallbacks.
pub(crate) mod auth;
mod device_rpc;
pub(crate) mod error;
mod plugin_rpc;
mod rpc;
#[cfg(test)]
mod rpc_tests;
pub(crate) mod state;
#[cfg(test)]
mod tests;

use crate::{api::error::ApiError, api::state::App};
use axum::{Router, http::StatusCode, middleware};

pub(crate) fn router(app: App) -> Router {
    Router::new()
        .nest_service("/rpc", rpc::service(app.clone()))
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

#[cfg(test)]
pub(crate) mod test_client;
